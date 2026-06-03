#[cfg(feature = "coverage")]
use std::path::PathBuf;

#[cfg(feature = "coverage")]
#[test]
fn coverage_clover_smoke() {
    let bin = env!("CARGO_BIN_EXE_phpunit-rust");
    let out = std::env::temp_dir().join("phpunit_rust_cov_smoke.json");
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample_project");
    // Debug builds may overflow the default 8 MB stack during coverage
    // serialisation.  64 MB is safe for both debug and release.
    let status = std::process::Command::new(bin)
        .env("RUST_MIN_STACK", "67108864")
        .args([
            "--project",
            project.to_str().unwrap(),
            "--workers",
            "1",
            "--coverage-format",
            "json",
            "--coverage-out",
            out.to_str().unwrap(),
        ])
        .status()
        .expect("failed to spawn phpunit-rust");
    // The fixture intentionally contains a failing test, so the runner exits
    // with code 1.  We only require that coverage was still written.
    let _ = status;
    let raw = std::fs::read_to_string(&out).expect("coverage output not written");
    let map: std::collections::HashMap<String, serde_json::Value> =
        serde_json::from_str(&raw).expect("coverage output is not valid JSON");
    assert!(
        !map.is_empty(),
        "coverage map must contain at least one file"
    );
    let _ = std::fs::remove_file(&out);
}

#[test]
fn fork_worker_php_script_exists_and_is_valid_syntax() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../php/worker_fork.php");
    assert!(
        script.exists(),
        "php/worker_fork.php not found at {:?}",
        script
    );

    let output = std::process::Command::new("php")
        .args(["-l", script.to_str().unwrap()])
        .output()
        .expect("php -l failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("No syntax errors"),
        "php -l failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn fork_pool_runs_fixture_and_streams_outcomes() {
    use phpunit_rust::fork_pool::PhpForkPool;
    use phpunit_rust::types::{BatchClass, BatchPlan};

    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample_project");
    let autoload = project.join("vendor/autoload.php");
    let script = phpunit_rust::php_worker::find_fork_script().expect("worker_fork.php not found");

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
        &std::collections::HashMap::new(),
        "512M",
        0,
    )
    .expect("PhpForkPool::spawn failed");

    pool.write_batch(
        0,
        &BatchPlan {
            autoload: autoload.clone(),
            bootstrap: None,
            defines: vec![],
            classes: vec![BatchClass {
                file: project.join("tests/SampleTest.php"),
                class: "SampleTest".to_string(),
                methods: vec![],
                row_filter: None,
                required_files: vec![],
                is_isolated: false,
            }],
            fingerprint: std::collections::HashSet::new(),
            force_exit_after: false,
        },
    )
    .expect("write_batch slot 0");

    pool.write_batch(
        1,
        &BatchPlan {
            autoload: autoload.clone(),
            bootstrap: None,
            defines: vec![],
            classes: vec![],
            fingerprint: std::collections::HashSet::new(),
            force_exit_after: false,
        },
    )
    .expect("write_batch slot 1");

    pool.close_write_ends();

    let readers = pool.into_readers();
    let mut all_outcomes: Vec<phpunit_rust::types::TestOutcome> = Vec::new();
    for mut reader in readers {
        use std::io::BufRead;
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                if let Ok(o) = serde_json::from_str::<phpunit_rust::types::TestOutcome>(trimmed) {
                    all_outcomes.push(o);
                }
            }
            line.clear();
        }
    }
    pool.wait();

    assert!(
        !all_outcomes.is_empty(),
        "expected at least one outcome from SampleTest"
    );
    let classes: std::collections::HashSet<&str> =
        all_outcomes.iter().map(|o| o.class.as_str()).collect();
    assert!(
        classes.contains("SampleTest"),
        "SampleTest outcomes missing; got: {classes:?}"
    );
}

/// C2: a worker that hangs (alive but never emits output) must NOT hang the
/// whole run. With `worker_timeout` set, the inactivity watchdog kills the
/// pool and reports the stuck test as an error within a bounded time.
///
/// The test guards itself with a 25 s wall-clock deadline: without the
/// watchdog the run would block on `sleep(3600)` and this test would fail at
/// the deadline rather than hanging the suite forever.
#[test]
fn worker_timeout_aborts_stuck_run() {
    use phpunit_rust::fork_pool::PhpForkPool;
    use phpunit_rust::provider_enum::RowCounts;
    use phpunit_rust::runner::{run, RunConfig};
    use phpunit_rust::types::{TestCase, TestStatus};
    use std::time::{Duration, Instant};

    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample_project");
    let autoload = project.join("vendor/autoload.php");
    let script = phpunit_rust::php_worker::find_fork_script().expect("worker_fork.php not found");

    // A test that blocks far longer than both the watchdog and the deadline.
    let dir = std::env::temp_dir().join(format!("phpunit_rust_hang_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let test_file = dir.join("HangTest.php");
    std::fs::write(
        &test_file,
        "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass HangTest extends TestCase {\n    public function testHang(): void { sleep(3600); }\n}\n",
    ).unwrap();

    let case = TestCase {
        file: test_file.clone(),
        class: "HangTest".to_string(),
        method: "testHang".to_string(),
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
            0,
        )
        .expect("PhpForkPool::spawn failed");
        let cfg = RunConfig {
            autoload: autoload_t.clone(),
            bootstrap: None,
            filter: None,
            defines: vec![],
            stop_on: Default::default(),
            class_file_index: std::collections::HashMap::new(),
            n_workers: 1,
            worker_timeout: Some(Duration::from_secs(2)),
        };
        run(&mut pool, vec![case], &cfg, &RowCounts::new(), |_o| {})
    });

    // Deadline guard: a broken watchdog fails the test instead of hanging.
    let deadline = Instant::now() + Duration::from_secs(25);
    while !handle.is_finished() {
        assert!(
            Instant::now() < deadline,
            "run did not return within 25s — the inactivity watchdog failed to fire"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let report = handle
        .join()
        .expect("run thread panicked")
        .expect("run returned Err");
    let _ = std::fs::remove_dir_all(&dir);

    let stuck = report
        .outcomes
        .iter()
        .find(|o| o.class == "HangTest" && o.status == TestStatus::Error)
        .unwrap_or_else(|| {
            panic!(
                "stuck HangTest must be reported as an Error; got: {:?}",
                report.outcomes
            )
        });
    assert!(
        stuck.message.as_deref().unwrap_or("").contains("watchdog"),
        "the synthesised error should mention the watchdog; got: {:?}",
        stuck.message
    );
}

/// M7: when a worker process dies mid-batch (crash, fatal error, OOM) the
/// master reports `slot_died`, and `run` must synthesise an Error outcome for
/// every test in the lost batch rather than silently dropping it. Exercises the
/// parity-preserving SlotDied recovery path in the dispatcher.
#[test]
fn worker_crash_is_recovered_as_error() {
    use phpunit_rust::fork_pool::PhpForkPool;
    use phpunit_rust::provider_enum::RowCounts;
    use phpunit_rust::runner::{run, RunConfig};
    use phpunit_rust::types::{TestCase, TestStatus};
    use std::time::{Duration, Instant};

    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample_project");
    let autoload = project.join("vendor/autoload.php");
    let script = phpunit_rust::php_worker::find_fork_script().expect("worker_fork.php not found");

    // A test that hard-kills its own worker process mid-run.
    let dir = std::env::temp_dir().join(format!("phpunit_rust_crash_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let test_file = dir.join("CrashTest.php");
    std::fs::write(
        &test_file,
        "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass CrashTest extends TestCase {\n    public function testCrash(): void { posix_kill(posix_getpid(), SIGKILL); }\n}\n",
    ).unwrap();

    let case = TestCase {
        file: test_file.clone(),
        class: "CrashTest".to_string(),
        method: "testCrash".to_string(),
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
            0,
        )
        .expect("PhpForkPool::spawn failed");
        // No watchdog: this must be recovered via the `slot_died` path, not the
        // inactivity timeout.
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

    // Deadline guard: if SlotDied recovery fails, the dispatcher would hang
    // waiting for outcomes that never arrive — fail the test instead.
    let deadline = Instant::now() + Duration::from_secs(25);
    while !handle.is_finished() {
        assert!(
            Instant::now() < deadline,
            "run did not return within 25s — SlotDied recovery failed to surface the crash"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let report = handle
        .join()
        .expect("run thread panicked")
        .expect("run returned Err");
    let _ = std::fs::remove_dir_all(&dir);

    let crashed = report
        .outcomes
        .iter()
        .find(|o| o.class == "CrashTest" && o.status == TestStatus::Error)
        .unwrap_or_else(|| {
            panic!(
                "crashed CrashTest must be reported as an Error; got: {:?}",
                report.outcomes
            )
        });
    assert!(
        crashed.message.as_deref().unwrap_or("").contains("worker"),
        "the synthesised error should mention the worker failure; got: {:?}",
        crashed.message
    );
}
