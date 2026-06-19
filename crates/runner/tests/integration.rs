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
        None,
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
            None,
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
            None,
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

/// Counted-at-most-once regression: a multi-class bin `[A(passes), B(fatals)]`
/// must yield A counted EXACTLY once as Pass and B counted EXACTLY once as
/// Error. Multi-class bins are the default dispatch shape, so when B fatals
/// mid-batch the dead-worker recovery used to synthesise a second (Error)
/// outcome for A (whose real passes had already streamed) and double-report B
/// (its `<class>` shutdown row PLUS Rust's per-method synth). This pins the fix:
/// the synth paths skip any (class, method) already reported.
///
/// A is deliberately heavier (more methods) so LPT orders it first in the bin
/// and it executes — and reports — before B triggers the uncatchable fatal.
#[test]
fn multi_class_bin_crash_counts_each_test_once() {
    use phpunit_rust::fork_pool::PhpForkPool;
    use phpunit_rust::provider_enum::RowCounts;
    use phpunit_rust::runner::{run, RunConfig};
    use phpunit_rust::types::{TestCase, TestStatus};
    use std::time::{Duration, Instant};

    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample_project");
    let autoload = project.join("vendor/autoload.php");
    let script = phpunit_rust::php_worker::find_fork_script().expect("worker_fork.php not found");

    let dir = std::env::temp_dir().join(format!("phpunit_rust_bin_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // Helper: build a lifecycle-flagged case (forces the class-level batching
    // path so classes flow into the LPT bin packer and can share one bin).
    let mk = |file: &std::path::Path, class: &str, method: &str| TestCase {
        file: file.to_path_buf(),
        class: class.to_string(),
        method: method.to_string(),
        data_provider: None,
        groups: vec![],
        external_providers: vec![],
        is_tautological: false,
        has_lifecycle_overrides: true,
        depends_on: vec![],
        is_dispatch_safe: true,
        fingerprint: std::collections::HashSet::new(),
        is_stateful: false,
        is_isolated: false,
        needs_db: false,
    };

    let mut cases: Vec<TestCase> = Vec::new();

    // Padding class: 12 passing methods. With 1 worker and 16 total methods the
    // LPT target is 4, so PadTest (cost 12 ≥ target) dispatches solo while the
    // lighter PassTest (3) and FatalTest (1) pack into ONE multi-class bin —
    // the default shape that triggered the double-count. (Distribution verified
    // against build_queue.)
    let pad_methods: Vec<String> = (0..12).map(|i| format!("testPad{i}")).collect();
    let pad_body: String = pad_methods
        .iter()
        .map(|m| format!("    public function {m}(): void {{ $this->assertTrue(true); }}\n"))
        .collect();
    let pad_file = dir.join("PadTest.php");
    std::fs::write(
        &pad_file,
        format!("<?php\nuse PHPUnit\\Framework\\TestCase;\nclass PadTest extends TestCase {{\n{pad_body}}}\n"),
    )
    .unwrap();
    for m in &pad_methods {
        cases.push(mk(&pad_file, "PadTest", m));
    }

    // Class A: three plain passing methods (LPT-orders it FIRST in the bin so it
    // runs and reports before the bin-mate fatals).
    let a_file = dir.join("PassTest.php");
    std::fs::write(
        &a_file,
        "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass PassTest extends TestCase {\n    public function testP1(): void { $this->assertTrue(true); }\n    public function testP2(): void { $this->assertTrue(true); }\n    public function testP3(): void { $this->assertTrue(true); }\n}\n",
    )
    .unwrap();
    for m in ["testP1", "testP2", "testP3"] {
        cases.push(mk(&a_file, "PassTest", m));
    }

    // Class B: triggers an uncatchable E_ERROR (undefined function) so the PHP
    // shutdown handler fires and emits the `<class>` fatal row, then the master
    // reports slot_died. This is the precise path that double-counted A.
    let b_file = dir.join("FatalTest.php");
    std::fs::write(
        &b_file,
        "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass FatalTest extends TestCase {\n    public function testFatal(): void { __phpunit_rust_no_such_function_xyz(); }\n}\n",
    )
    .unwrap();
    cases.push(mk(&b_file, "FatalTest", "testFatal"));

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
            None,
        )
        .expect("PhpForkPool::spawn failed");
        // n_workers=1 with 16 total methods → target=4: PadTest dispatches solo,
        // PassTest (3) + FatalTest (1) pack into ONE multi-class BatchPlan to the
        // lone worker — the default shape that exercised the double-count.
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
        run(&mut pool, cases, &cfg, &RowCounts::new(), |_o| {})
    });

    let deadline = Instant::now() + Duration::from_secs(25);
    while !handle.is_finished() {
        assert!(
            Instant::now() < deadline,
            "run did not return within 25s — crash recovery hung"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let report = handle
        .join()
        .expect("run thread panicked")
        .expect("run returned Err");
    let _ = std::fs::remove_dir_all(&dir);

    // PassTest: every one of its three methods counted EXACTLY once, all Pass.
    // (The bug re-emitted them as Error after the bin-mate fataled.)
    for m in ["testP1", "testP2", "testP3"] {
        let rows: Vec<_> = report
            .outcomes
            .iter()
            .filter(|o| o.class == "PassTest" && o.method == m)
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "PassTest::{m} must be counted exactly once; got: {rows:?}"
        );
        assert_eq!(
            rows[0].status,
            TestStatus::Pass,
            "PassTest::{m} must stay green, not flip to Error; got: {:?}",
            rows[0]
        );
    }

    // FatalTest: counted EXACTLY once, as Error. The single row is the PHP
    // shutdown handler's `<class>` row (carrying the fatal text) — Rust must NOT
    // add a redundant per-method synth for the same class.
    let fatal_rows: Vec<_> = report
        .outcomes
        .iter()
        .filter(|o| o.class == "FatalTest")
        .collect();
    assert_eq!(
        fatal_rows.len(),
        1,
        "FatalTest must be reported exactly once (no double report); got: {fatal_rows:?}"
    );
    assert_eq!(
        fatal_rows[0].status,
        TestStatus::Error,
        "FatalTest must be an Error; got: {:?}",
        fatal_rows[0]
    );

    // PadTest: all 12 methods counted exactly once as Pass (the solo batch
    // runs cleanly and must be untouched by the crash recovery).
    let pad_passes = report
        .outcomes
        .iter()
        .filter(|o| o.class == "PadTest" && o.status == TestStatus::Pass)
        .count();
    assert_eq!(pad_passes, 12, "PadTest's 12 passes must all survive");

    // Total parity: 12 PadTest + 3 PassTest passes + 1 FatalTest error = 16
    // outcomes, each test accounted for exactly once (vanilla-equivalent
    // expansion). The bug inflated this with redundant per-method Error synths.
    assert_eq!(
        report.outcomes.len(),
        16,
        "total outcomes must equal the vanilla expansion (16), no inflation; got: {:?}",
        report.outcomes
    );
}

#[test]
fn inline_master_crash_reports_remaining_tests_as_errors_no_hang() {
    use phpunit_rust::fork_pool::PhpForkPool;
    use phpunit_rust::provider_enum::RowCounts;
    use phpunit_rust::runner::{run, RunConfig};
    use phpunit_rust::types::{TestCase, TestStatus};
    use std::time::{Duration, Instant};
    // L3 crash-safety: in the single-process (no-fork) path the master IS the worker, so a fatal in
    // a test kills the master itself — there is NO surviving master to emit slot_died. The run loop's
    // EOF path must still synthesise an error for the lost in-flight batch and terminate, never hang.
    let autoload = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/sample_project/vendor/autoload.php");
    if !autoload.is_file() {
        eprintln!("SKIP: sample_project vendor not installed");
        return;
    }
    let script = phpunit_rust::php_worker::find_fork_script().expect("worker_fork.php not found");
    let dir =
        std::env::temp_dir().join(format!("phpunit_rust_inline_crash_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // An uncatchable E_ERROR (undefined function) kills the inline master process mid-batch.
    let fatal_file = dir.join("InlineFatalTest.php");
    std::fs::write(
        &fatal_file,
        "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass InlineFatalTest extends TestCase {\n    public function testFatal(): void { __phpunit_rust_inline_no_such_fn_xyz(); }\n}\n",
    )
    .unwrap();

    let mk = |file: &std::path::Path, class: &str, method: &str| TestCase {
        file: file.to_path_buf(),
        class: class.to_string(),
        method: method.to_string(),
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
    let cases = vec![mk(&fatal_file, "InlineFatalTest", "testFatal")];

    let autoload_t = autoload.clone();
    let handle = std::thread::spawn(move || {
        // spawn_inline: the master runs the per-batch loop itself (no fork), single slot.
        let mut pool = PhpForkPool::spawn_inline(
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
            None,
        )
        .expect("spawn_inline failed");
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
        run(&mut pool, cases, &cfg, &RowCounts::new(), |_o| {})
    });

    // Deadline guard: a broken inline-EOF path would hang the dispatcher waiting for outcomes the
    // dead master can never send.
    let deadline = Instant::now() + Duration::from_secs(25);
    while !handle.is_finished() {
        assert!(
            Instant::now() < deadline,
            "inline run did not return within 25s — the master crash hung the dispatcher"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let report = handle
        .join()
        .expect("run thread panicked")
        .expect("run returned Err");
    let _ = std::fs::remove_dir_all(&dir);

    // The fatal class must be reported (not silently lost), as an Error — via the PHP shutdown
    // handler's `<class>` row or the EOF synth path.
    let fatal_rows: Vec<_> = report
        .outcomes
        .iter()
        .filter(|o| o.class == "InlineFatalTest")
        .collect();
    assert!(
        !fatal_rows.is_empty(),
        "InlineFatalTest must be reported, not silently lost; outcomes: {:?}",
        report.outcomes
    );
    assert!(
        fatal_rows.iter().all(|o| o.status == TestStatus::Error),
        "InlineFatalTest must be an Error after the inline master crash; got: {fatal_rows:?}"
    );
}

#[test]
fn inline_runs_all_batches_without_voluntary_recycle() {
    use phpunit_rust::fork_pool::PhpForkPool;
    use phpunit_rust::provider_enum::RowCounts;
    use phpunit_rust::runner::{run, RunConfig};
    use phpunit_rust::types::{TestCase, TestStatus};
    use std::time::{Duration, Instant};
    // Regression: the inline (no-fork) path must NOT honour the K-batch voluntary recycle. There is
    // no master to fork a warm replacement, so a recycle would orphan every batch past K — the
    // process just exits and the runner synthesises "worker crashed" errors for the rest. With
    // max_batches_per_child=1 and 3 single-class batches, the pre-fix inline worker recycled after
    // batch 1 (WORKER_EXIT_VOLUNTARY_RECYCLE) and lost classes 2 and 3. All 3 must now pass.
    let autoload = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/sample_project/vendor/autoload.php");
    if !autoload.is_file() {
        eprintln!("SKIP: sample_project vendor not installed");
        return;
    }
    let script = phpunit_rust::php_worker::find_fork_script().expect("worker_fork.php not found");
    let dir = std::env::temp_dir().join(format!(
        "phpunit_rust_inline_norecycle_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let mut files = Vec::new();
    for n in 1..=3 {
        let class = format!("InlineBatch{n}Test");
        let f = dir.join(format!("{class}.php"));
        std::fs::write(
            &f,
            format!(
                "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass {class} extends TestCase {{\n    public function testOk(): void {{ $this->assertTrue(true); }}\n}}\n"
            ),
        )
        .unwrap();
        files.push((f, class));
    }

    let mk = |file: &std::path::Path, class: &str| TestCase {
        file: file.to_path_buf(),
        class: class.to_string(),
        method: "testOk".to_string(),
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
    let cases: Vec<TestCase> = files.iter().map(|(f, c)| mk(f, c)).collect();

    let autoload_t = autoload.clone();
    let handle = std::thread::spawn(move || {
        // max_batches_per_child = 1: pre-fix this made the inline worker recycle after the FIRST
        // batch. The fix passes 0 (unlimited) to runChild on the inline path, so all batches run.
        let mut pool = PhpForkPool::spawn_inline(
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
        .expect("spawn_inline failed");
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
        run(&mut pool, cases, &cfg, &RowCounts::new(), |_o| {})
    });

    let deadline = Instant::now() + Duration::from_secs(25);
    while !handle.is_finished() {
        assert!(
            Instant::now() < deadline,
            "inline run did not return within 25s"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let report = handle
        .join()
        .expect("run thread panicked")
        .expect("run returned Err");
    let _ = std::fs::remove_dir_all(&dir);

    for (_f, class) in &files {
        let rows: Vec<_> = report
            .outcomes
            .iter()
            .filter(|o| &o.class == class)
            .collect();
        assert!(
            !rows.is_empty(),
            "{class} was lost (orphaned by a voluntary recycle?); outcomes: {:?}",
            report.outcomes
        );
        assert!(
            rows.iter().all(|o| o.status == TestStatus::Pass),
            "{class} must Pass on the inline path (no recycle death); got: {rows:?}"
        );
    }
}

/// Phase 1 — SIGKILL-based proof that the child-death re-queue salvages the innocent tail.
///
/// An undefined-function call would NOT work as poison here: in PHP 8 it surfaces as a catchable
/// `Error` that PHPUnit's `catch (\Throwable)` swallows, so the forked CHILD never actually dies,
/// never exercising the SlotDied → `requeue_unrun` salvage path. This test uses an uncatchable death.
///
/// This test pokes the real wiring: `testPoison` sends SIGKILL to its own PID, which is
/// UNCATCHABLE. The forked child dies mid-batch, the master emits `slot_died`, and the runner's
/// `requeue_unrun` re-queues the batch's un-run methods onto a fresh child. The DISTINGUISHING
/// signal is the poison message: `testPoison` ends as `Error` with "poison pill" in its message,
/// which ONLY `requeue_unrun` emits when a test reaches the MAX_ATTEMPTS cap (killed → re-queued
/// isolated → killed again). If `testPoison` is `Error` WITHOUT "poison pill", the death/requeue
/// path did NOT fire and Phase 1 is broken.
///
/// All of this happens inside ONE `run` call: the master forks the replacement child, so there is
/// no master death and no manual respawn loop.
///
/// CRITICAL: this requires FORK-SERVER mode (max_batches_per_child > 0). In long-lived mode
/// (max_batches_per_child = 0) the master closes its copies of the
/// child FDs and blocks on a plain `pcntl_waitpid` with NO SIGCHLD handler, so a child death
/// surfaces to Rust only as EOF, never `slot_died` — and `requeue_unrun` cannot fire. The salvage
/// mechanism is therefore exercised ONLY when the runner runs the master as a fork-server.
#[test]
fn forked_salvage_via_sigkill_proves_requeue() {
    use phpunit_rust::fork_pool::PhpForkPool;
    use phpunit_rust::provider_enum::RowCounts;
    use phpunit_rust::runner::{run, RunConfig};
    use phpunit_rust::types::{TestCase, TestStatus};
    use std::time::{Duration, Instant};

    let autoload = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/sample_project/vendor/autoload.php");
    if !autoload.is_file() {
        eprintln!("SKIP: sample_project vendor not installed");
        return;
    }
    let script = phpunit_rust::php_worker::find_fork_script().expect("worker_fork.php not found");
    let dir =
        std::env::temp_dir().join(format!("phpunit_rust_salvage_kill_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // testOk1 passes, testPoison SIGKILLs the forked child (uncatchable — unlike an undefined fn),
    // testOk2 must still run after the master forks a replacement child and re-queues it.
    let file = dir.join("SalvageKillTest.php");
    std::fs::write(
        &file,
        "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass SalvageKillTest extends TestCase {\n  public function testOk1(): void { $this->assertTrue(true); }\n  public function testPoison(): void { posix_kill(posix_getpid(), 9); }\n  public function testOk2(): void { $this->assertTrue(true); }\n}\n",
    )
    .unwrap();

    let mk = |method: &str| TestCase {
        file: file.clone(),
        class: "SalvageKillTest".to_string(),
        method: method.to_string(),
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
    let cases = vec![mk("testOk1"), mk("testPoison"), mk("testOk2")];

    let autoload_t = autoload.clone();
    let handle = std::thread::spawn(move || {
        // Fork-server mode (max_batches_per_child = 1, NOT 0): the SlotDied → requeue_unrun
        // salvage only exists in fork-server mode. In long-lived mode (0) the master closes its
        // copies of the child FDs and blocks on pcntl_waitpid with NO SIGCHLD handler, so a child
        // death surfaces only as EOF — never `slot_died` — and the re-queue path cannot fire.
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
        .expect("spawn failed");
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
        run(&mut pool, cases, &cfg, &RowCounts::new(), |_o| {})
    });

    let deadline = Instant::now() + Duration::from_secs(25);
    while !handle.is_finished() {
        assert!(Instant::now() < deadline, "salvage-via-sigkill run hung");
        std::thread::sleep(Duration::from_millis(100));
    }
    let report = handle.join().expect("panicked").expect("Err");
    let _ = std::fs::remove_dir_all(&dir);

    let outcome_of = |m: &str| report.outcomes.iter().find(|o| o.method == m);
    let status_of = |m: &str| outcome_of(m).map(|o| o.status.clone());

    // testOk1 ran and reported before the kill.
    assert_eq!(
        status_of("testOk1"),
        Some(TestStatus::Pass),
        "testOk1 must pass; outcomes: {:?}",
        report.outcomes
    );
    // The innocent tail must be SALVAGED — re-queued onto the fresh replacement child.
    assert_eq!(
        status_of("testOk2"),
        Some(TestStatus::Pass),
        "testOk2 (innocent tail) must be salvaged via re-queue onto a fresh child; outcomes: {:?}",
        report.outcomes
    );
    // The poison test must end as Error AT THE CAP. The "poison pill" message is the
    // distinguishing signal that the SlotDied → requeue_unrun → MAX_ATTEMPTS path genuinely fired
    // (death → re-queue isolated → death again). An Error WITHOUT it means the kill/requeue path
    // never ran — Phase 1 would be broken/vacuous.
    let poison = outcome_of("testPoison");
    assert_eq!(
        poison.map(|o| o.status.clone()),
        Some(TestStatus::Error),
        "testPoison must end as Error at the cap; outcomes: {:?}",
        report.outcomes
    );
    let poison_msg = poison.and_then(|o| o.message.clone()).unwrap_or_default();
    assert!(
        poison_msg.contains("poison pill"),
        "testPoison's Error must come from requeue_unrun's MAX_ATTEMPTS cap (message must contain \
         \"poison pill\"); without it the child-death re-queue salvage never fired. Got message: \
         {poison_msg:?}; outcomes: {:?}",
        report.outcomes
    );

    for m in ["testOk1", "testPoison", "testOk2"] {
        assert_eq!(
            report.outcomes.iter().filter(|o| o.method == m).count(),
            1,
            "{m} must be reported exactly once; outcomes: {:?}",
            report.outcomes
        );
    }
}

/// Phase 2 — transient master death: when an inline master dies with work still queued, the
/// queued-but-never-dispatched tests come back as `unfinished` from `run_resumable`. After
/// re-spawning a fresh pool and re-running all unfinished tests, they pass and nothing is left
/// unfinished.
///
/// Mechanism: the inline master IS the worker. A test that sends `SIGKILL` to its own PID
/// (`posix_kill(getmypid(), 9)`) kills the PHP process immediately — SIGKILL is uncatchable
/// and no shutdown handler runs. The runner sees EOF with work still in-flight or queued;
/// `master_died = true` and ALL unfinished tests (not yet received) are returned as `unfinished`.
/// A marker-file guard makes the kill transient: the marker is written BEFORE the kill, so on
/// the second run the test sees the marker and passes instead of killing.
///
/// Setup: two separate PHP classes → two separate batches.
/// P2TransientKillTest kills the master on the first run only (marker-guarded).
/// P2InnocentAfterTest is still in the queue when EOF fires. BOTH come back as `unfinished`.
/// On the second run (respawn) both pass — this is the core "transient recovery" proof.
#[test]
fn master_death_transient_recovered_by_respawn() {
    use phpunit_rust::fork_pool::PhpForkPool;
    use phpunit_rust::profiler::Profiler;
    use phpunit_rust::provider_enum::RowCounts;
    use phpunit_rust::runner::{run_resumable, RunConfig};
    use phpunit_rust::types::{TestCase, TestStatus};
    use std::time::{Duration, Instant};

    let autoload = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/sample_project/vendor/autoload.php");
    if !autoload.is_file() {
        eprintln!("SKIP: sample_project vendor not installed");
        return;
    }
    let script = phpunit_rust::php_worker::find_fork_script().expect("worker_fork.php not found");
    let dir =
        std::env::temp_dir().join(format!("phpunit_rust_p2_transient_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("killed_once");

    // Class A: on the FIRST run, writes a marker then sends SIGKILL to itself (killing the
    // inline master instantly — no shutdown handler). On subsequent runs (marker exists), it
    // just asserts true and passes. This makes the master death transient.
    let kill_file = dir.join("P2TransientKillTest.php");
    std::fs::write(
        &kill_file,
        format!(
            "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass P2TransientKillTest extends TestCase {{\n  public function testTransient(): void {{\n    $m = '{marker}';\n    if (!file_exists($m)) {{ file_put_contents($m, '1'); posix_kill(getmypid(), 9); }}\n    $this->assertTrue(true);\n  }}\n}}\n",
            marker = marker.to_string_lossy()
        ),
    )
    .unwrap();

    // Class B: always passes — queued AFTER A. When A kills the master, B is never dispatched
    // and is returned as `unfinished` (along with A, since SIGKILL emits no shutdown outcome).
    let innocent_file = dir.join("P2InnocentAfterTest.php");
    std::fs::write(
        &innocent_file,
        "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass P2InnocentAfterTest extends TestCase {\n  public function testOk(): void { $this->assertTrue(true); }\n}\n",
    )
    .unwrap();

    let mk_kill = || TestCase {
        file: kill_file.clone(),
        class: "P2TransientKillTest".to_string(),
        method: "testTransient".to_string(),
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
    let mk_innocent = || TestCase {
        file: innocent_file.clone(),
        class: "P2InnocentAfterTest".to_string(),
        method: "testOk".to_string(),
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

    // Helper: spawn a fresh inline pool, run run_resumable, return (outcomes, unfinished).
    // Guarded by a 25 s wall-clock deadline so a hang fails the test instead of blocking the suite.
    let run_once =
        |cases: Vec<TestCase>| -> (Vec<phpunit_rust::types::TestOutcome>, Vec<TestCase>) {
            let script = script.clone();
            let autoload = autoload.clone();
            let handle = std::thread::spawn(move || {
                let cfg = RunConfig {
                    autoload: autoload.clone(),
                    bootstrap: None,
                    filter: None,
                    defines: vec![],
                    stop_on: Default::default(),
                    class_file_index: std::collections::HashMap::new(),
                    n_workers: 1,
                    worker_timeout: None,
                };
                let prof = Profiler::new(false);
                let mut pool = PhpForkPool::spawn_inline(
                    &script,
                    &autoload,
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
                    None,
                )
                .expect("spawn_inline failed");
                let noop: fn(&phpunit_rust::types::TestOutcome) = |_o| {};
                run_resumable(&mut pool, cases, &cfg, &RowCounts::new(), noop, &prof)
                    .map(|(r, u)| (r.outcomes, u))
                    .expect("run_resumable returned Err")
            });
            let deadline = Instant::now() + Duration::from_secs(25);
            while !handle.is_finished() {
                assert!(Instant::now() < deadline, "run_resumable hung");
                std::thread::sleep(Duration::from_millis(100));
            }
            handle.join().expect("thread panicked")
        };

    // Run 1: P2TransientKillTest sends SIGKILL → master dies → no shutdown handler → both A and
    // B had no outcomes received → both come back as unfinished.
    let (out1, unfinished1) = run_once(vec![mk_kill(), mk_innocent()]);
    assert!(
        !unfinished1.is_empty(),
        "master death (SIGKILL) must return unfinished tests; \
         out1={out1:?} unfinished1={unfinished1:?}"
    );
    assert!(
        unfinished1.iter().any(|c| c.class == "P2InnocentAfterTest"),
        "the queued-but-undispatched innocent class must be in unfinished1; \
         unfinished1={unfinished1:?}"
    );

    // Run 2 (respawn): marker file now exists → kill test passes; innocent test passes.
    // Nothing is left unfinished — this is the core "transient recovery" proof.
    let (out2, unfinished2) = run_once(unfinished1);
    assert!(
        unfinished2.is_empty(),
        "second run (respawn) must complete with nothing unfinished; unfinished2={unfinished2:?}"
    );
    assert!(
        out2.iter()
            .any(|o| o.class == "P2InnocentAfterTest" && o.status == TestStatus::Pass),
        "the queued test must pass on respawn; out2={out2:?}"
    );
    // The previously-killing test must now pass (marker prevents the kill).
    assert!(
        out2.iter()
            .any(|o| o.class == "P2TransientKillTest" && o.status == TestStatus::Pass),
        "the transient-killer test must pass after respawn (marker exists); out2={out2:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Phase 2 — deterministic master-killer is bounded: a test that ALWAYS sends SIGKILL to its
/// own PID keeps killing the master every respawn. A loop capped at MAX_RESPAWNS must terminate
/// under a wall-clock deadline (no-hang invariant). The key invariant: NO HANG, bounded
/// iteration count, and no silent test loss at the cap.
///
/// Structure: Class A (always sends SIGKILL, always dispatched first) kills the master every
/// time. Since SIGKILL triggers no shutdown handler, no outcome is emitted; both A and B
/// (still in the queue) come back as `unfinished` each iteration. The loop artificially
/// re-injects A each time to simulate a permanent master-killer. After MAX_RESPAWNS the loop
/// stops — the key invariant is NO HANG and bounded iteration count.
#[test]
fn master_death_always_is_bounded_no_hang() {
    use phpunit_rust::fork_pool::PhpForkPool;
    use phpunit_rust::profiler::Profiler;
    use phpunit_rust::provider_enum::RowCounts;
    use phpunit_rust::runner::{run_resumable, RunConfig};
    use phpunit_rust::types::TestCase;
    use std::time::{Duration, Instant};

    let autoload = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/sample_project/vendor/autoload.php");
    if !autoload.is_file() {
        eprintln!("SKIP: sample_project vendor not installed");
        return;
    }
    let script = phpunit_rust::php_worker::find_fork_script().expect("worker_fork.php not found");
    let dir = std::env::temp_dir().join(format!("phpunit_rust_p2_always_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // Class A: always sends SIGKILL to itself — kills the master every time, no recovery.
    let fatal_file = dir.join("P2AlwaysKillTest.php");
    std::fs::write(
        &fatal_file,
        "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass P2AlwaysKillTest extends TestCase {\n  public function testBoom(): void { posix_kill(getmypid(), 9); }\n}\n",
    )
    .unwrap();

    // Class B: always passes but never gets to run because A kills the master first.
    let innocent_file = dir.join("P2BoundedInnocentTest.php");
    std::fs::write(
        &innocent_file,
        "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass P2BoundedInnocentTest extends TestCase {\n  public function testOk(): void { $this->assertTrue(true); }\n}\n",
    )
    .unwrap();

    let mk_kill = || TestCase {
        file: fatal_file.clone(),
        class: "P2AlwaysKillTest".to_string(),
        method: "testBoom".to_string(),
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
    let mk_innocent = || TestCase {
        file: innocent_file.clone(),
        class: "P2BoundedInnocentTest".to_string(),
        method: "testOk".to_string(),
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

    const MAX_RESPAWNS: usize = 3;
    let overall_deadline = Instant::now() + Duration::from_secs(60);

    let mut respawn_count = 0usize;

    loop {
        assert!(
            Instant::now() < overall_deadline,
            "bounded respawn loop exceeded 60 s deadline after {respawn_count} respawns"
        );

        // Always inject [A, B]: A kills the master, B comes back as unfinished.
        // This simulates the worst-case permanent master-killer driving repeated respawns.
        let cases = vec![mk_kill(), mk_innocent()];
        let script = script.clone();
        let autoload = autoload.clone();
        let handle = std::thread::spawn(move || {
            let cfg = RunConfig {
                autoload: autoload.clone(),
                bootstrap: None,
                filter: None,
                defines: vec![],
                stop_on: Default::default(),
                class_file_index: std::collections::HashMap::new(),
                n_workers: 1,
                worker_timeout: None,
            };
            let prof = Profiler::new(false);
            let mut pool = PhpForkPool::spawn_inline(
                &script,
                &autoload,
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
                None,
            )
            .expect("spawn_inline failed");
            let noop: fn(&phpunit_rust::types::TestOutcome) = |_o| {};
            run_resumable(&mut pool, cases, &cfg, &RowCounts::new(), noop, &prof)
                .map(|(r, u)| (r.outcomes, u))
                .expect("run_resumable returned Err")
        });

        // Per-iteration deadline.
        let iter_deadline = Instant::now() + Duration::from_secs(20);
        while !handle.is_finished() {
            assert!(
                Instant::now() < iter_deadline,
                "run_resumable hung on iteration {respawn_count}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        let (_outcomes, unfinished) = handle.join().expect("thread panicked");

        respawn_count += 1;
        if respawn_count >= MAX_RESPAWNS {
            // Cap reached: assert no silent loss — the queued test is still unfinished.
            assert!(
                unfinished
                    .iter()
                    .any(|c| c.class == "P2BoundedInnocentTest"),
                "after cap the queued test must still be unfinished (no silent loss); \
                 unfinished={unfinished:?}"
            );
            break;
        }

        if unfinished.is_empty() {
            // Unexpectedly all tests accounted for — no hang and terminated early, still valid.
            break;
        }
    }

    assert!(
        respawn_count <= MAX_RESPAWNS,
        "respawn loop must be bounded: ran {respawn_count} iterations, cap={MAX_RESPAWNS}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
