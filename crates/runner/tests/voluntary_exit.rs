//! Finding (2): a test/provider/teardown that calls `exit(0)`/`die()` mid-batch
//! must NOT be mistaken for a voluntary K-batch recycle.
//!
//! In fork-server mode (`max_batches_per_child > 0`, or a `force_exit_after`
//! batch) the child exits voluntarily after finishing its work, and the master
//! forks a replacement that inherits the warm state. The master used to signal
//! voluntariness *implicitly* via exit code 0. A test calling `exit(0)`
//! mid-batch is therefore indistinguishable from a clean recycle: the master
//! forks a replacement (which blocks on stdin), emits NO `slot_died`, and the
//! run stalls until the 600 s watchdog mass-errors everything.
//!
//! The fix makes voluntary recycles EXPLICIT via a reserved exit code (6). The
//! master treats ONLY the reserved codes (6 = recycle, 7 = stdin EOF) as
//! voluntary; any other exit — including a bare `exit(0)` — is a crash, which
//! the master telegraphs as `slot_died` so Rust synthesises an Error outcome
//! for the lost batch instead of hanging.
//!
//! Rust needs no change: it only ever consumes `slot_died` notices (it never
//! sees raw child exit codes), and the existing SlotDied recovery path already
//! handles `exit_code: 0`. This test guards the end-to-end behaviour.

use std::time::{Duration, Instant};

/// A test that calls `exit(0)` mid-batch, run in fork-server mode, must be
/// recovered as an Error within a bounded time — never hang the run.
#[test]
fn voluntary_exit0_midbatch_is_recovered_not_clean_recycle() {
    use phpunit_rust::fork_pool::PhpForkPool;
    use phpunit_rust::provider_enum::RowCounts;
    use phpunit_rust::runner::{run, RunConfig};
    use phpunit_rust::types::{TestCase, TestStatus};

    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample_project");
    let autoload = project.join("vendor/autoload.php");
    let script = phpunit_rust::php_worker::find_fork_script().expect("worker_fork.php not found");

    // A test method that calls exit(0) — exactly what a misbehaving
    // provider/teardown/test does. PHPUnit never gets to emit an outcome.
    let dir = std::env::temp_dir().join(format!("phpunit_rust_die_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let test_file = dir.join("DieTest.php");
    std::fs::write(
        &test_file,
        "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass DieTest extends TestCase {\n    public function testDie(): void { exit(0); }\n}\n",
    )
    .unwrap();

    let case = TestCase {
        file: test_file.clone(),
        class: "DieTest".to_string(),
        method: "testDie".to_string(),
        data_provider: None,
        groups: vec![],
        external_providers: vec![],
        is_tautological: false,
        has_lifecycle_overrides: false,
        depends_on: vec![],
        is_dispatch_safe: true,
        fingerprint: std::collections::HashSet::new(),
        is_stateful: false,
        is_isolated: false,
        needs_db: false,
    };

    let autoload_t = autoload.clone();
    let handle = std::thread::spawn(move || {
        // max_batches_per_child = 1 puts the master in fork-server mode, where
        // the SIGCHLD respawn path runs and the exit-0-as-clean-recycle bug
        // lives. Without the fix the master forks a replacement and never
        // emits slot_died → the dispatcher hangs forever.
        let mut pool = PhpForkPool::spawn(
            &script,
            &autoload_t,
            None,
            &[],
            &[],
            &[],
            &[],
            &[],
            1,
            &std::collections::HashMap::new(),
            "512M",
            1,
            None,
        )
        .expect("PhpForkPool::spawn failed");
        // No watchdog: this must be recovered via the `slot_died` path, NOT the
        // inactivity timeout. A passing test here proves the master named the
        // mid-batch die() a crash rather than a clean recycle.
        let cfg = RunConfig {
            autoload: autoload_t.clone(),
            bootstrap: None,
            filter: None,
            defines: vec![],
            stop_on: Default::default(),
            class_file_index: std::collections::HashMap::new(),
            n_workers: 1,
            worker_timeout: None,
        };
        run(&mut pool, vec![case], &cfg, &RowCounts::new(), |_o| {})
    });

    // Deadline guard: if the master treats exit(0) as a clean recycle, the
    // dispatcher waits for outcomes that never arrive — fail fast instead of
    // hanging the suite.
    let deadline = Instant::now() + Duration::from_secs(25);
    while !handle.is_finished() {
        assert!(
            Instant::now() < deadline,
            "run did not return within 25s — exit(0) mid-batch was mistaken for a clean \
             K-batch recycle (no slot_died emitted), so the dispatcher hung"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let report = handle
        .join()
        .expect("run thread panicked")
        .expect("run returned Err");
    let _ = std::fs::remove_dir_all(&dir);

    let died = report
        .outcomes
        .iter()
        .find(|o| o.class == "DieTest" && o.status == TestStatus::Error)
        .unwrap_or_else(|| {
            panic!(
                "a test calling exit(0) mid-batch must surface as an Error; got: {:?}",
                report.outcomes
            )
        });

    // With worker-death resilience the crashed test is re-queued and hits the
    // MAX_ATTEMPTS poison-pill cap rather than being immediately errored with
    // the raw exit code.  The key invariant is that an Error is surfaced
    // (checked above) and the run does not hang.
    let msg = died.message.as_deref().unwrap_or("");
    assert!(
        msg.contains("worker died") || msg.contains("exit code") || msg.contains("poison pill"),
        "the Error message must reference the worker death cause; got: {msg:?}"
    );
}
