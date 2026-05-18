use phpunit_rust::client::WorkerClient;
use phpunit_rust::frankenphp::{find_worker_script, FrankenPhp};
use phpunit_rust::types::{TestRequest, TestStatus};
use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample_project")
}

#[test]
fn runs_a_passing_test_end_to_end() {
    let worker = find_worker_script().expect("worker.php must exist");
    let fph = FrankenPhp::spawn(&worker).expect("frankenphp must spawn");
    let client = WorkerClient::new(fph.worker_url());

    let root = fixture_root();
    let req = TestRequest {
        autoload: root.join("vendor/autoload.php"),
        file: root.join("tests/CalculatorTest.php"),
        class: "Sample\\Tests\\CalculatorTest".into(),
        method: "testAddsTwoPositiveIntegers".into(),
    };

    let outcome = client.run_test(&req).expect("worker call must succeed");
    assert_eq!(outcome.status, TestStatus::Pass, "outcome was: {outcome:?}");
}

#[test]
fn reports_a_failing_test_as_fail() {
    let worker = find_worker_script().expect("worker.php must exist");
    let fph = FrankenPhp::spawn(&worker).expect("frankenphp must spawn");
    let client = WorkerClient::new(fph.worker_url());

    let root = fixture_root();
    let req = TestRequest {
        autoload: root.join("vendor/autoload.php"),
        file: root.join("tests/FailingTest.php"),
        class: "Sample\\Tests\\FailingTest".into(),
        method: "testThisDeliberatelyFails".into(),
    };

    let outcome = client.run_test(&req).expect("worker call must succeed");
    assert_eq!(outcome.status, TestStatus::Fail);
    assert!(outcome.message.as_deref().unwrap_or("").contains("intentional"));
}
