//! Integration test: namespace resolution.
//!
//! Validates that Phase 2.5 resolves `new Service()` in `namespace App\Tests;`
//! (with `use App\Service;`) to the FQCN `App\Service`, finds the class in
//! mago-project's reflection, and recurses into `App\Service::go()` body —
//! marking its lines as covered.
//!
//! Without Phase 2.5, this test would fail because the walker would store
//! `Type::Class("Service")` and fail to find a class with that name (the
//! actual FQCN is `App\Service`).

use assert_cmd::Command;

const FIXTURE: &str = "tests/fixtures/with_namespaces";

#[test]
fn namespaces_fixture_runs_clean() {
    Command::cargo_bin("pcov-rs")
        .unwrap()
        .current_dir(FIXTURE)
        .args(["analyze", "--format", "pcov-extended"])
        .assert()
        .success();
}

#[test]
fn namespaced_service_method_is_covered() {
    let output = Command::cargo_bin("pcov-rs")
        .unwrap()
        .current_dir(FIXTURE)
        .args(["analyze", "--format", "pcov-extended"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let map = parsed.as_object().expect("top-level object");

    // Service.php should be in coverage.
    let service_key = map
        .keys()
        .find(|k| k.ends_with("Service.php") && !k.contains("Test"));
    assert!(
        service_key.is_some(),
        "src/App/Service.php should be in coverage (namespace resolution worked); keys: {:?}",
        map.keys().collect::<Vec<_>>()
    );

    // Its lines should be attributed to the namespaced test class.
    let service_lines = parsed[service_key.unwrap()].as_object().unwrap();
    let attributed = service_lines.values().any(|tests| {
        tests.as_array().unwrap().iter().any(|v| {
            v.as_str()
                .is_some_and(|s| s.contains("App\\Tests\\ServiceTest::testGo"))
        })
    });
    assert!(
        attributed,
        "Service.php should have at least one line attributed to App\\Tests\\ServiceTest::testGo"
    );
}
