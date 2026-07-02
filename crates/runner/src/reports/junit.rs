//! PHPUnit-compatible JUnit XML report.
//!
//! Structure mirrors PHPUnit's `--log-junit`: a root `<testsuites>` wrapping one
//! aggregate `<testsuite>`, which nests one `<testsuite>` per test class, each
//! holding `<testcase>` elements. Failures/errors/skips become the corresponding
//! child element. Counts and `time` (seconds) are aggregated per class and at the
//! root. See the module docs for the deliberately-omitted attributes.

use super::group_by_class;
use crate::runner::Report;
use crate::types::{TestOutcome, TestStatus};
use std::fmt::Write as _;

/// Render `report` as a PHPUnit-compatible JUnit XML document. `suite_name` names
/// the root `<testsuite>` (empty string is fine and matches a common PHPUnit run).
#[must_use]
pub fn junit_xml(report: &Report, suite_name: &str) -> String {
    let groups = group_by_class(report);

    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<testsuites>\n");

    let root_skipped = report.skipped() + report.incomplete();
    let _ = writeln!(
        out,
        "  <testsuite name=\"{}\" tests=\"{}\" errors=\"{}\" failures=\"{}\" skipped=\"{}\" time=\"{:.6}\">",
        attr(suite_name),
        report.outcomes.len(),
        report.errored(),
        report.failed(),
        root_skipped,
        report.total_duration_ms / 1000.0,
    );

    for (class, cases) in &groups {
        let errors = cases
            .iter()
            .filter(|o| o.status == TestStatus::Error)
            .count();
        let failures = cases
            .iter()
            .filter(|o| o.status == TestStatus::Fail)
            .count();
        let skipped = cases
            .iter()
            .filter(|o| matches!(o.status, TestStatus::Skipped | TestStatus::Incomplete))
            .count();
        let time: f64 = cases.iter().map(|o| o.duration_ms).sum::<f64>() / 1000.0;
        let _ = writeln!(
            out,
            "    <testsuite name=\"{}\" tests=\"{}\" errors=\"{}\" failures=\"{}\" skipped=\"{}\" time=\"{:.6}\">",
            attr(class),
            cases.len(),
            errors,
            failures,
            skipped,
            time,
        );
        for outcome in cases {
            out.push_str(&testcase_xml(outcome));
        }
        out.push_str("    </testsuite>\n");
    }

    out.push_str("  </testsuite>\n");
    out.push_str("</testsuites>\n");
    out
}

fn testcase_xml(outcome: &TestOutcome) -> String {
    // Match PHPUnit's data-set naming: numeric rows -> `#0`, named rows -> `"name"`.
    let name = match &outcome.dataset {
        Some(ds) if ds.parse::<u64>().is_ok() => {
            format!("{} with data set #{}", outcome.method, ds)
        }
        Some(ds) => format!("{} with data set \"{}\"", outcome.method, ds),
        None => outcome.method.clone(),
    };
    // PHPUnit's `classname` is the FQCN with `\` replaced by `.` (used by CI tools
    // like Jenkins for package.class grouping); `class` keeps the backslashes.
    let classname = outcome.class.replace('\\', ".");
    let open = format!(
        "      <testcase name=\"{}\" class=\"{}\" classname=\"{}\" time=\"{:.6}\"",
        attr(&name),
        attr(&outcome.class),
        attr(&classname),
        outcome.duration_ms / 1000.0,
    );
    match outcome.status {
        // Risky is reported as a plain (passing) testcase — JUnit has no risky
        // element, matching how consumers treat PHPUnit's risky results.
        TestStatus::Pass | TestStatus::Risky => format!("{open}/>\n"),
        // JUnit has no "incomplete": PHPUnit folds it into skipped.
        TestStatus::Skipped | TestStatus::Incomplete => {
            format!("{open}>\n        <skipped/>\n      </testcase>\n")
        }
        TestStatus::Fail => format!(
            "{open}>\n        <failure type=\"{}\">{}</failure>\n      </testcase>\n",
            attr("PHPUnit\\Framework\\ExpectationFailedException"),
            text(&body(outcome)),
        ),
        TestStatus::Error => format!(
            "{open}>\n        <error type=\"{}\">{}</error>\n      </testcase>\n",
            attr("Error"),
            text(&body(outcome)),
        ),
    }
}

/// The failure/error text body: message and stack trace, as PHPUnit emits.
fn body(outcome: &TestOutcome) -> String {
    match (&outcome.message, &outcome.trace) {
        (Some(m), Some(t)) => format!("{m}\n\n{t}"),
        (Some(m), None) => m.clone(),
        (None, Some(t)) => t.clone(),
        (None, None) => String::new(),
    }
}

fn attr(s: &str) -> String {
    escape(s, true)
}

fn text(s: &str) -> String {
    escape(s, false)
}

/// XML-escape for text content (`& < >`) or, when `is_attr`, also `"`.
fn escape(s: &str, is_attr: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if is_attr => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn report(outcomes: Vec<TestOutcome>) -> Report {
        Report {
            outcomes,
            total_duration_ms: 5.0,
        }
    }

    #[test]
    fn passing_test_is_a_self_closing_testcase() {
        let r = report(vec![outcome("App\\FooTest", "testBar", TestStatus::Pass)]);
        let xml = junit_xml(&r, "");
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        assert!(xml.contains("<testcase name=\"testBar\" class=\"App\\FooTest\" classname=\"App.FooTest\" time=\"0.001000\"/>"));
        // no failure/error/skipped child for a pass
        assert!(!xml.contains("<failure"));
        assert!(!xml.contains("<skipped"));
    }

    #[test]
    fn counts_are_aggregated_at_class_and_root() {
        let r = report(vec![
            outcome("App\\FooTest", "testA", TestStatus::Pass),
            outcome("App\\FooTest", "testB", TestStatus::Fail),
            outcome("App\\FooTest", "testC", TestStatus::Error),
            outcome("App\\FooTest", "testD", TestStatus::Skipped),
            outcome("App\\FooTest", "testE", TestStatus::Incomplete),
        ]);
        let xml = junit_xml(&r, "suite");
        // root
        assert!(xml.contains(
            "<testsuite name=\"suite\" tests=\"5\" errors=\"1\" failures=\"1\" skipped=\"2\""
        ));
        // per-class
        assert!(xml.contains(
            "<testsuite name=\"App\\FooTest\" tests=\"5\" errors=\"1\" failures=\"1\" skipped=\"2\""
        ));
    }

    #[test]
    fn failure_error_and_skipped_render_their_child_elements() {
        let mut fail = outcome("C", "testF", TestStatus::Fail);
        fail.message = Some("Failed asserting that 1 matches 2".to_string());
        let mut err = outcome("C", "testE", TestStatus::Error);
        err.message = Some("boom".to_string());
        err.trace = Some("#0 file.php(1)".to_string());
        let skip = outcome("C", "testS", TestStatus::Skipped);
        let xml = junit_xml(&report(vec![fail, err, skip]), "");
        assert!(xml.contains("<failure type=\"PHPUnit\\Framework\\ExpectationFailedException\">Failed asserting that 1 matches 2</failure>"));
        assert!(xml.contains("<error type=\"Error\">boom\n\n#0 file.php(1)</error>"));
        assert!(xml.contains("<skipped/>"));
    }

    #[test]
    fn special_characters_are_escaped() {
        let mut fail = outcome("C", "testX", TestStatus::Fail);
        fail.message = Some("expected <a> & got \"b\" > c".to_string());
        let xml = junit_xml(&report(vec![fail]), "");
        // text content: & < > escaped, quotes left as-is inside text
        assert!(xml.contains("expected &lt;a&gt; &amp; got \"b\" &gt; c"));
    }

    #[test]
    fn numeric_dataset_uses_hash_named_dataset_uses_quotes() {
        // Matching PHPUnit: numeric row -> `#0`, named row -> `"zeros"` (quotes
        // XML-escaped in the attribute).
        let mut num = outcome("C", "testRows", TestStatus::Pass);
        num.dataset = Some("0".to_string());
        let mut named = outcome("C", "testRows", TestStatus::Pass);
        named.dataset = Some("zeros".to_string());
        let xml = junit_xml(&report(vec![num, named]), "");
        assert!(xml.contains("name=\"testRows with data set #0\""));
        assert!(xml.contains("name=\"testRows with data set &quot;zeros&quot;\""));
    }

    #[test]
    fn each_class_gets_its_own_nested_testsuite_in_order() {
        let r = report(vec![
            outcome("Zeta", "t1", TestStatus::Pass),
            outcome("Alpha", "t2", TestStatus::Pass),
            outcome("Zeta", "t3", TestStatus::Pass),
        ]);
        let xml = junit_xml(&r, "");
        // Zeta first (first-seen), grouped (2 cases), Alpha second
        let zeta = xml.find("name=\"Zeta\"").unwrap();
        let alpha = xml.find("name=\"Alpha\"").unwrap();
        assert!(zeta < alpha, "classes must keep first-seen order");
        assert!(xml.contains("<testsuite name=\"Zeta\" tests=\"2\""));
    }
}
