use phpunit_rust::client::WorkerClient;
use phpunit_rust::frankenphp::{find_worker_script, FrankenPhp};
use phpunit_rust::types::{TestRunRequest, TestStatus};
use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample_project")
}

fn request(file: &str, class: &str) -> TestRunRequest {
    let root = fixture_root();
    TestRunRequest {
        autoload: root.join("vendor/autoload.php"),
        phpunit_xml: None,
        file: root.join(file),
        class: class.into(),
        methods: vec![],
    }
}

#[test]
fn calculator_class_all_three_methods_pass() {
    let worker = find_worker_script().expect("worker.php must exist");
    let fph = FrankenPhp::spawn(&worker).expect("frankenphp must spawn");
    let client = WorkerClient::new(fph.worker_url());

    let req = request("tests/CalculatorTest.php", "Sample\\Tests\\CalculatorTest");
    let outcomes = client.run_class(&req).expect("worker call must succeed");

    // Including testDivisionByZeroThrows — now passes because PHPUnit's
    // real runner handles expectException correctly.
    assert_eq!(outcomes.len(), 3, "outcomes: {outcomes:?}");
    for o in &outcomes {
        assert_eq!(o.status, TestStatus::Pass, "{}::{} was {:?}: {:?}", o.class, o.method, o.status, o.message);
    }
}

#[test]
fn failing_class_mixed_results() {
    let worker = find_worker_script().expect("worker.php must exist");
    let fph = FrankenPhp::spawn(&worker).expect("frankenphp must spawn");
    let client = WorkerClient::new(fph.worker_url());

    let req = request("tests/FailingTest.php", "Sample\\Tests\\FailingTest");
    let outcomes = client.run_class(&req).expect("worker call must succeed");

    assert_eq!(outcomes.len(), 2);
    let by_method: std::collections::HashMap<_, _> = outcomes.iter().map(|o| (o.method.clone(), o)).collect();
    assert_eq!(by_method["testThisPasses"].status, TestStatus::Pass);
    assert_eq!(by_method["testThisDeliberatelyFails"].status, TestStatus::Fail);
    assert!(by_method["testThisDeliberatelyFails"].message.as_deref().unwrap_or("").contains("intentional"));
}
