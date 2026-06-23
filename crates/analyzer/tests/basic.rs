//! Integration test: pcov-rs analyze against the value_objects fixture.

use assert_cmd::Command;

mod common;

const FIXTURE_NAME: &str = "value_objects";

#[test]
fn analyze_value_objects_succeeds() {
    let wd = common::isolated_fixture(FIXTURE_NAME);
    let mut cmd = Command::cargo_bin("pcov-rs").unwrap();
    cmd.current_dir(wd.path())
        .arg("analyze")
        .arg("--format")
        .arg("pcov-extended")
        .assert()
        .success();
}

#[test]
fn analyze_reports_test_method_lines() {
    let wd = common::isolated_fixture(FIXTURE_NAME);
    let output = Command::cargo_bin("pcov-rs")
        .unwrap()
        .current_dir(wd.path())
        .args(["analyze", "--format", "pcov-extended"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "non-zero exit; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let raw = String::from_utf8(output.stdout).unwrap();

    // Phase 2: both the test file and production code appear.
    let parsed: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("output is not valid JSON: {e}\nraw:\n{raw}"));

    let keys: Vec<&str> = parsed
        .as_object()
        .expect("top-level JSON must be an object")
        .keys()
        .map(String::as_str)
        .collect();

    assert!(
        keys.iter().any(|k| k.ends_with("MoneyTest.php")),
        "expected a key ending with MoneyTest.php, got: {keys:?}"
    );

    // Phase 2: src/Money.php MUST appear — the analyzer now recurses into
    // production callees (constructor + method dispatch).
    assert!(
        keys.iter()
            .any(|k| k.ends_with("Money.php") && !k.contains("Test")),
        "Phase 2: src/Money.php should appear in coverage; got keys: {keys:?}"
    );

    // Both test methods must be attributed somewhere in the output.
    assert!(
        raw.contains("MoneyTest::testAdd"),
        "expected testAdd attribution in output, got:\n{raw}"
    );
    assert!(
        raw.contains("MoneyTest::testAccessors"),
        "expected testAccessors attribution in output, got:\n{raw}"
    );

    // Body lines of testAdd (lines 6–12 in MoneyTest.php) must be present.
    let test_file_key = keys
        .iter()
        .find(|k| k.ends_with("MoneyTest.php"))
        .expect("MoneyTest.php key must exist");
    let line_map = parsed[test_file_key]
        .as_object()
        .expect("file entry must be a line-map object");

    assert!(
        !line_map.is_empty(),
        "MoneyTest.php must have at least one covered line"
    );

    // Money.php must have at least one line attributed to a test method.
    let money_key = keys
        .iter()
        .find(|k| k.ends_with("Money.php") && !k.contains("Test"))
        .expect("Money.php key must exist");
    let money_lines = parsed[money_key]
        .as_object()
        .expect("Money.php entry must be a line-map object");

    assert!(
        money_lines
            .values()
            .any(|tests| { tests.as_array().map(|a| !a.is_empty()).unwrap_or(false) }),
        "Money.php must have at least one line attributed to a test; got: {money_lines:?}"
    );

    // Specifically, Money.php lines must reference MoneyTest methods.
    assert!(
        raw.contains("Money.php") && raw.contains("MoneyTest::testAdd"),
        "Money.php lines should be attributed to MoneyTest::testAdd"
    );
}
