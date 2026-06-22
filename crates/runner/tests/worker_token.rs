use phpunit_rust::fork_pool::PhpForkPool;
use phpunit_rust::types::{BatchClass, BatchPlan, TestOutcome};
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::PathBuf;

/// Write a temp TestCase whose single test FAILS with the worker token as the
/// message, so the token is carried back in TestOutcome.message. getenv() is
/// the same channel the PHP hot path will check, so this proves visibility to
/// a *running test*, not just to the worker bootstrap.
fn write_echo_test(dir: &std::path::Path) -> PathBuf {
    let file = dir.join("WorkerTokenEchoTest.php");
    std::fs::write(
        &file,
        "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass WorkerTokenEchoTest extends TestCase {\n    public function testEchoToken(): void {\n        $this->fail('TOKEN=' . var_export(getenv('PHPUNIT_RUST_WORKER_ID'), true));\n    }\n}\n",
    )
    .unwrap();
    file
}

fn drain(reader: &mut impl BufRead, out: &mut Vec<TestOutcome>) {
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        let t = line.trim();
        if !t.is_empty() {
            if let Ok(o) = serde_json::from_str::<TestOutcome>(t) {
                out.push(o);
            }
        }
        line.clear();
    }
}

fn plan_for(file: &std::path::Path) -> BatchPlan {
    BatchPlan {
        classes: vec![BatchClass {
            file: file.to_path_buf(),
            class: "WorkerTokenEchoTest".to_string(),
            methods: vec![],
            row_filter: None,
            required_files: vec![],
            is_isolated: false,
        }],
        fingerprint: HashSet::new(),
        force_exit_after: false,
    }
}

fn token_in(outcomes: &[TestOutcome]) -> String {
    let msg = outcomes
        .iter()
        .find(|o| o.class == "WorkerTokenEchoTest")
        .and_then(|o| o.message.clone())
        .expect("WorkerTokenEchoTest outcome with a message");
    let i = msg.find("TOKEN=").expect("TOKEN= marker in message");
    msg[i + "TOKEN=".len()..].trim().to_string()
}

/// Token must be present and DISTINCT across slots.
#[test]
fn worker_token_is_set_and_distinct_per_slot() {
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample_project");
    let autoload = project.join("vendor/autoload.php");
    let script = phpunit_rust::php_worker::find_fork_script().expect("worker_fork.php");
    let dir = std::env::temp_dir().join(format!("phpunit_rust_token_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = write_echo_test(&dir);

    let mut pool = PhpForkPool::spawn(
        &script,
        &autoload,
        None,
        &[],
        &[],
        &[],
        &[],
        &[],
        2,
        &HashMap::new(),
        "512M",
        0,
        None,
    )
    .expect("spawn");
    pool.write_batch(0, &plan_for(&file)).unwrap();
    pool.write_batch(1, &plan_for(&file)).unwrap();
    pool.close_write_ends();

    let mut per_slot: Vec<String> = Vec::new();
    for mut reader in pool.into_readers() {
        let mut outs = Vec::new();
        drain(&mut reader, &mut outs);
        if !outs.is_empty() {
            per_slot.push(token_in(&outs));
        }
    }
    pool.wait();
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(per_slot.len(), 2, "both slots produced an outcome");
    for t in &per_slot {
        assert_ne!(
            t, "false",
            "PHPUNIT_RUST_WORKER_ID must be set (getenv != false)"
        );
    }
    let distinct: HashSet<&String> = per_slot.iter().collect();
    assert_eq!(
        distinct.len(),
        2,
        "tokens must be distinct per slot: {per_slot:?}"
    );
}

/// Token must SURVIVE a K-batch recycle: with max_batches_per_child=1 the
/// child exits after batch 1, the master respawns it on the same slot, and
/// a second batch must report the identical token.
///
/// DISPATCH PROTOCOL: the runner sends plan N+1 only after plan N completes
/// (batch_done JSON). We mirror that here: write batch 1, block-read until
/// we get an outcome (child 0 ran and exited → master respawned), THEN write
/// batch 2, close, and drain. Writing both batches up-front would race PHP's
/// stream-read buffer and silently discard plan 2 when child 0 exits.
#[test]
fn worker_token_stable_across_recycle() {
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample_project");
    let autoload = project.join("vendor/autoload.php");
    let script = phpunit_rust::php_worker::find_fork_script().expect("worker_fork.php");
    let dir = std::env::temp_dir().join(format!("phpunit_rust_token_rcy_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = write_echo_test(&dir);

    // max_batches_per_child = 1 -> fork-server mode, child recycles after
    // each batch via the SIGCHLD path that re-invokes $forkChildForSlot($slot).
    let mut pool = PhpForkPool::spawn(
        &script,
        &autoload,
        None,
        &[],
        &[],
        &[],
        &[],
        &[],
        1,
        &HashMap::new(),
        "512M",
        1,
        None,
    )
    .expect("spawn");

    // Batch 1: write, take reader, block-read ONE outcome (proves child ran).
    pool.write_batch(0, &plan_for(&file)).unwrap();
    let mut reader = pool.take_reader(0).expect("reader for slot 0");
    let mut outs: Vec<TestOutcome> = Vec::new();
    // Read lines until we collect at least one WorkerTokenEchoTest outcome.
    // The pool still owns the write-end so the pipe stays open; we only need
    // to read until we see the batch-1 outcome (a `fail` JSON line).
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut line = String::new();
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for batch-1 outcome"
            );
            let n = reader.read_line(&mut line).unwrap_or(0);
            if n == 0 {
                break; // EOF — shouldn't happen yet, but be safe
            }
            let t = line.trim();
            if !t.is_empty() {
                if let Ok(o) = serde_json::from_str::<TestOutcome>(t) {
                    let is_our_class = o.class == "WorkerTokenEchoTest";
                    outs.push(o);
                    if is_our_class {
                        break; // Got batch-1 outcome; proceed to batch 2
                    }
                }
            }
            line.clear();
        }
    }
    assert!(!outs.is_empty(), "batch 1 produced no outcome");

    // Batch 2: child 0 has already exited and master has respawned child 0b.
    pool.write_batch(0, &plan_for(&file)).unwrap();
    pool.close_write_ends();

    // Drain remaining (batch-2 outcome + any trailing lines).
    drain(&mut reader, &mut outs);
    pool.wait();
    let _ = std::fs::remove_dir_all(&dir);

    let tokens: Vec<String> = outs
        .iter()
        .filter(|o| o.class == "WorkerTokenEchoTest")
        .filter_map(|o| o.message.clone())
        .filter_map(|m| {
            m.find("TOKEN=")
                .map(|i| m[i + "TOKEN=".len()..].trim().to_string())
        })
        .collect();
    assert!(
        tokens.len() >= 2,
        "both batches ran (recycle happened): {tokens:?}"
    );
    assert!(
        tokens.iter().all(|t| t == "'0'"),
        "slot-0 token stable across recycle: {tokens:?}"
    );
}
