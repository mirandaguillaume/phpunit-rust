use phpunit_rust::client::WorkerClient;
use phpunit_rust::frankenphp::{find_worker_script, WorkerPool};
use phpunit_rust::types::{TestRunRequest, TestStatus};
use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample_project")
}

fn request(file: &str, class: &str) -> TestRunRequest {
    let root = fixture_root();
    TestRunRequest {
        autoload: root.join("vendor/autoload.php"),
        bootstrap: None,
        file: root.join(file),
        class: class.into(),
        methods: vec![],
        defines: vec![],
        describe_only: false,
        row_filter: None,
    }
}

#[test]
#[ignore = "integration tests ported to PhpWorkerPool in Task 7; FrankenPHP worker.php removed"]
fn calculator_class_all_three_methods_pass() {
    let worker = find_worker_script().expect("worker.php must exist");
    let pool = WorkerPool::spawn(&worker, 1).expect("1-worker pool must spawn");
    let client = WorkerClient::new(pool.urls().first().unwrap().clone());

    let req = request("tests/CalculatorTest.php", "Sample\\Tests\\CalculatorTest");
    let outcomes = client.run_class(&req).expect("worker call must succeed");

    assert_eq!(outcomes.len(), 3, "outcomes: {outcomes:?}");
    for o in &outcomes {
        assert_eq!(o.status, TestStatus::Pass, "{}::{} was {:?}: {:?}", o.class, o.method, o.status, o.message);
    }
}

#[test]
#[ignore = "integration tests ported to PhpWorkerPool in Task 7; FrankenPHP worker.php removed"]
fn failing_class_mixed_results() {
    let worker = find_worker_script().expect("worker.php must exist");
    let pool = WorkerPool::spawn(&worker, 1).expect("pool must spawn");
    let client = WorkerClient::new(pool.urls().first().unwrap().clone());

    let req = request("tests/FailingTest.php", "Sample\\Tests\\FailingTest");
    let outcomes = client.run_class(&req).expect("worker call must succeed");

    assert_eq!(outcomes.len(), 2);
    let by_method: std::collections::HashMap<_, _> = outcomes.iter().map(|o| (o.method.clone(), o)).collect();
    assert_eq!(by_method["testThisPasses"].status, TestStatus::Pass);
    assert_eq!(by_method["testThisDeliberatelyFails"].status, TestStatus::Fail);
    assert!(by_method["testThisDeliberatelyFails"].message.as_deref().unwrap_or("").contains("intentional"));
}

#[test]
#[ignore = "integration tests ported to PhpWorkerPool in Task 7; FrankenPHP worker.php removed"]
fn pool_of_three_serves_three_distinct_classes_concurrently() {
    // Sanity check: a 3-worker pool can serve 3 different class requests
    // without errors. We don't measure speed here; just correctness.
    let worker = find_worker_script().expect("worker.php must exist");
    let pool = WorkerPool::spawn(&worker, 3).expect("3-worker pool must spawn");
    assert_eq!(pool.len(), 3);

    let urls = pool.urls();
    let clients: Vec<WorkerClient> =
        urls.iter().map(|u| WorkerClient::new(u.clone())).collect();

    let r1 = request("tests/CalculatorTest.php", "Sample\\Tests\\CalculatorTest");
    let r2 = request("tests/FailingTest.php", "Sample\\Tests\\FailingTest");
    let r3 = request("tests/DataProviderTest.php", "Sample\\Tests\\DataProviderTest");

    let o1 = clients[0].run_class(&r1).expect("client 0 ok");
    let o2 = clients[1].run_class(&r2).expect("client 1 ok");
    let o3 = clients[2].run_class(&r3).expect("client 2 ok");

    assert_eq!(o1.len(), 3, "CalculatorTest outcomes");
    assert_eq!(o2.len(), 2, "FailingTest outcomes");
    assert_eq!(o3.len(), 4, "DataProviderTest outcomes (4 data rows)");
}
