use crate::runner::Report;
use crate::types::{TestOutcome, TestStatus};
use colored::Colorize;

pub fn print_progress(outcome: &TestOutcome) {
    let mark = match outcome.status {
        TestStatus::Pass => ".".green(),
        TestStatus::Fail => "F".red(),
        TestStatus::Error => "E".yellow(),
    };
    print!("{mark}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

pub fn print_summary(report: &Report) {
    println!();
    println!();
    for outcome in &report.outcomes {
        match outcome.status {
            TestStatus::Pass => {}
            TestStatus::Fail | TestStatus::Error => {
                let label = if matches!(outcome.status, TestStatus::Fail) { "FAIL" } else { "ERROR" };
                let color = if matches!(outcome.status, TestStatus::Fail) {
                    label.red().bold()
                } else {
                    label.yellow().bold()
                };
                println!("{color}  {}::{}", outcome.class, outcome.method);
                if let Some(msg) = &outcome.message {
                    for line in msg.lines() {
                        println!("    {line}");
                    }
                }
                if let Some(trace) = &outcome.trace {
                    for line in trace.lines().take(5) {
                        println!("    {}", line.dimmed());
                    }
                }
                println!();
            }
        }
    }
    let p = report.passed();
    let f = report.failed();
    let e = report.errored();
    let total = report.outcomes.len();
    let line = format!(
        "Tests: {total} total, {} passed, {} failed, {} errored ({:.1}ms)",
        p, f, e, report.total_duration_ms
    );
    if report.is_success() {
        println!("{}", line.green().bold());
    } else {
        println!("{}", line.red().bold());
    }
}
