//! Integration test: instanceof narrowing.
//!
//! Validates that Phase 2 narrows `$a instanceof Dog` → `Dog` inside the if
//! body, and that the `$a->bark()` call inside resolves to Dog::bark, which
//! gets traced (its body line covered). Cat::purr is never reached because
//! the test only narrows to Dog.

use assert_cmd::Command;

const FIXTURE: &str = "tests/fixtures/with_instanceof";

#[test]
fn instanceof_fixture_runs_clean() {
    Command::cargo_bin("pcov-rs")
        .unwrap()
        .current_dir(FIXTURE)
        .args(["analyze", "--format", "pcov-extended"])
        .assert()
        .success();
}

#[test]
fn instanceof_narrowing_covers_dog_bark() {
    let output = Command::cargo_bin("pcov-rs")
        .unwrap()
        .current_dir(FIXTURE)
        .args(["analyze", "--format", "pcov-extended"])
        .output()
        .unwrap();
    assert!(output.status.success(),
        "stderr: {}", String::from_utf8_lossy(&output.stderr));

    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let map = parsed.as_object().expect("top-level object");

    // Dog.php should be in coverage (its bark() method called via narrowed receiver).
    let dog_key = map.keys().find(|k| k.ends_with("Dog.php"))
        .expect("Dog.php should be in coverage");
    let dog_lines = parsed[dog_key].as_object().unwrap();
    let has_bark_attribution = dog_lines.values().any(|tests| {
        tests.as_array().unwrap().iter()
            .any(|v| v.as_str().map_or(false, |s| s.contains("testNarrowsToDog")))
    });
    assert!(has_bark_attribution,
        "Dog.php should have at least one line attributed to testNarrowsToDog (the bark() call traces into Dog::bark via narrowed receiver)");

    // Cat.php should NOT be in coverage (test only narrowed to Dog).
    let cat_present = map.keys().any(|k| k.ends_with("Cat.php"));
    assert!(!cat_present,
        "Cat.php should not be covered — the test only narrowed to Dog");
}
