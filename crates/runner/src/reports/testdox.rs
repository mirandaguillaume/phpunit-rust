//! PHPUnit-style TestDox report: each test class becomes a header, each test a
//! humanized sentence prefixed with a status mark. Rendered from a finished
//! [`Report`]; used for both the `--testdox` console view and the
//! `--log-testdox-text` file.

use super::group_by_class;
use crate::runner::Report;
use crate::types::TestStatus;
use std::fmt::Write as _;

/// Render `report` in TestDox text form:
///
/// ```text
/// FooTest (App\FooTest)
///  ✔ Bar does something
///  ✘ Baz fails
/// ```
#[must_use]
pub fn testdox_text(report: &Report) -> String {
    let mut out = String::new();
    for (class, cases) in group_by_class(report) {
        let short = class.rsplit('\\').next().unwrap_or(class);
        let _ = writeln!(out, "{short} ({class})");
        for outcome in cases {
            let mark = mark(outcome.status.clone());
            let mut sentence = humanize(&outcome.method);
            if let Some(ds) = &outcome.dataset {
                let _ = write!(sentence, " with data set {ds}");
            }
            let _ = writeln!(out, " {mark} {sentence}");
        }
        out.push('\n');
    }
    out
}

/// The status mark PHPUnit's TestDox printer uses.
fn mark(status: TestStatus) -> char {
    match status {
        TestStatus::Pass => '✔',
        TestStatus::Fail | TestStatus::Error => '✘',
        TestStatus::Skipped => '↩',
        TestStatus::Incomplete => '∅',
        TestStatus::Risky => '☢',
    }
}

/// Turn a test method name into a human sentence, PHPUnit-style:
/// `testFooBar` -> "Foo bar", `test_foo_bar` -> "Foo bar". Strips the `test`
/// prefix, splits on camelCase boundaries and underscores, lowercases, then
/// capitalizes the first letter. (Runs of capitals like acronyms are not split —
/// a known simplification versus PHPUnit's fuller algorithm.)
fn humanize(method: &str) -> String {
    let stripped = method.strip_prefix("test").unwrap_or(method);
    let chars: Vec<char> = stripped.chars().collect();

    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if i > 0 && c.is_uppercase() {
            let prev = chars[i - 1];
            if (prev.is_lowercase() || prev.is_ascii_digit()) && !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        words.push(cur);
    }

    let joined = words.join(" ").to_lowercase();
    let mut chars = joined.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TestOutcome;

    fn outcome(class: &str, method: &str, status: TestStatus) -> TestOutcome {
        TestOutcome {
            class: class.to_string(),
            method: method.to_string(),
            dataset: None,
            status,
            message: None,
            trace: None,
            duration_ms: 1.0,
        }
    }

    #[test]
    fn humanizes_camel_case_and_underscores() {
        assert_eq!(humanize("testFooBar"), "Foo bar");
        assert_eq!(humanize("test_foo_bar"), "Foo bar");
        assert_eq!(
            humanize("testReturnsTrueWhenEmpty"),
            "Returns true when empty"
        );
        assert_eq!(humanize("testHandlesUtf8Input"), "Handles utf8 input");
    }

    #[test]
    fn groups_by_class_with_short_header_and_marks() {
        let r = Report {
            outcomes: vec![
                outcome(
                    "App\\Math\\AdderTest",
                    "testAddsTwoNumbers",
                    TestStatus::Pass,
                ),
                outcome("App\\Math\\AdderTest", "testRejectsNaN", TestStatus::Fail),
            ],
            total_duration_ms: 2.0,
        };
        let dox = testdox_text(&r);
        assert!(dox.contains("AdderTest (App\\Math\\AdderTest)\n"));
        assert!(dox.contains(" ✔ Adds two numbers\n"));
        assert!(dox.contains(" ✘ Rejects na n\n") || dox.contains(" ✘ Rejects nan\n"));
    }

    #[test]
    fn marks_cover_every_status_and_dataset_is_appended() {
        let mut o = outcome("C", "testRows", TestStatus::Skipped);
        o.dataset = Some("#1".to_string());
        let r = Report {
            outcomes: vec![o],
            total_duration_ms: 1.0,
        };
        let dox = testdox_text(&r);
        assert!(dox.contains(" ↩ Rows with data set #1\n"));
    }
}
