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
            "--project", project.to_str().unwrap(),
            "--workers", "1",
            "--coverage-format", "json",
            "--coverage-out", out.to_str().unwrap(),
        ])
        .status()
        .expect("failed to spawn phpunit-rust");
    // The fixture intentionally contains a failing test, so the runner exits
    // with code 1.  We only require that coverage was still written.
    let _ = status;
    let raw = std::fs::read_to_string(&out).expect("coverage output not written");
    let map: std::collections::HashMap<String, serde_json::Value> =
        serde_json::from_str(&raw).expect("coverage output is not valid JSON");
    assert!(!map.is_empty(), "coverage map must contain at least one file");
    let _ = std::fs::remove_file(&out);
}

#[test]
fn fork_worker_php_script_exists_and_is_valid_syntax() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../php/worker_fork.php");
    assert!(script.exists(), "php/worker_fork.php not found at {:?}", script);

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

    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/sample_project");
    let autoload = project.join("vendor/autoload.php");
    let script = phpunit_rust::php_worker::find_fork_script()
        .expect("worker_fork.php not found");

    let mut pool = PhpForkPool::spawn(&script, &autoload, None, &[], &[], &[], &[], 2, &std::collections::HashMap::new(), "512M")
        .expect("PhpForkPool::spawn failed");

    pool.write_batch(0, &BatchPlan {
        autoload: autoload.clone(),
        bootstrap: None,
        defines: vec![],
        classes: vec![BatchClass {
            file:           project.join("tests/SampleTest.php"),
            class:          "SampleTest".to_string(),
            methods:        vec![],
            row_filter:     None,
            required_files: vec![],
        }],
    }).expect("write_batch slot 0");

    pool.write_batch(1, &BatchPlan {
        autoload: autoload.clone(),
        bootstrap: None,
        defines: vec![],
        classes: vec![],
    }).expect("write_batch slot 1");

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

    assert!(!all_outcomes.is_empty(), "expected at least one outcome from SampleTest");
    let classes: std::collections::HashSet<&str> =
        all_outcomes.iter().map(|o| o.class.as_str()).collect();
    assert!(classes.contains("SampleTest"),
        "SampleTest outcomes missing; got: {classes:?}");
}
