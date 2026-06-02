//! Integration test: data providers expand into multiple test invocations.
//!
//! Phase 2: production code (Calculator) DOES appear in coverage — each data
//! row instantiates Calculator and calls add(), which the analyzer traces.
//! Test method body lines are attributed to all THREE data rows, and
//! Calculator.php lines are attributed to all three data set names.

use assert_cmd::Command;

const FIXTURE: &str = "tests/fixtures/with_data_providers";

#[test]
fn data_provider_fixture_runs_clean() {
    Command::cargo_bin("pcov-rs")
        .unwrap()
        .current_dir(FIXTURE)
        .args(["analyze", "--format", "pcov-extended"])
        .assert()
        .success();
}

#[test]
fn data_provider_expands_to_three_cases() {
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
    let map = parsed.as_object().expect("object at top level");

    // Find the CalculatorTest.php entry.
    let (test_file_key, test_file_lines) = map
        .iter()
        .find(|(k, _)| k.ends_with("CalculatorTest.php"))
        .expect("expected CalculatorTest.php in output");

    let test_file_obj = test_file_lines.as_object().unwrap();

    // At least one line in the test file body should be attributed to all 3 data sets.
    let mut found_all_three = false;
    for (line_num, attribution) in test_file_obj.iter() {
        let ids: Vec<&str> = attribution
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let has_zero = ids
            .iter()
            .any(|s| s.starts_with("CalculatorTest::testAdd#") && s.contains("zero_plus_one"));
        let has_one = ids
            .iter()
            .any(|s| s.starts_with("CalculatorTest::testAdd#") && s.contains("one_plus_one"));
        let has_neg = ids
            .iter()
            .any(|s| s.starts_with("CalculatorTest::testAdd#") && s.contains("negatives"));
        if has_zero && has_one && has_neg {
            found_all_three = true;
            break;
        }
        // Debug: show what's on each line
        eprintln!("line {line_num}: {ids:?}");
    }

    assert!(found_all_three,
        "expected at least one line covered by all 3 data set invocations on testAdd (zero_plus_one, one_plus_one, negatives); file key was: {test_file_key}");

    // Phase 2: Calculator.php must appear in coverage — each data row calls
    // `new Calculator()` and `$calc->add(...)`, which the analyzer traces.
    let (calc_key, calc_lines_val) = map
        .iter()
        .find(|(k, _)| k.ends_with("Calculator.php") && !k.contains("Test"))
        .expect("Phase 2: Calculator.php should appear in coverage (production callee traced)");

    let calc_lines = calc_lines_val.as_object().unwrap();

    // At least one line in Calculator.php must be attributed to all 3 data sets.
    let mut calc_has_all_three = false;
    for (_line_num, attribution) in calc_lines.iter() {
        let ids: Vec<&str> = attribution
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let has_zero = ids.iter().any(|s| s.contains("zero_plus_one"));
        let has_one = ids.iter().any(|s| s.contains("one_plus_one"));
        let has_neg = ids.iter().any(|s| s.contains("negatives"));
        if has_zero && has_one && has_neg {
            calc_has_all_three = true;
            break;
        }
    }
    assert!(calc_has_all_three,
        "Calculator.php must have at least one line attributed to all 3 data sets; file key was: {calc_key}, lines: {calc_lines:?}");
}
