use crate::runner::Report;
use crate::types::{TestOutcome, TestStatus};
use colored::Colorize;

pub fn print_progress(outcome: &TestOutcome) {
    let mark = match outcome.status {
        TestStatus::Pass => ".".green(),
        TestStatus::Fail => "F".red(),
        TestStatus::Error => "E".yellow(),
        TestStatus::Skipped => "S".cyan(),
        TestStatus::Incomplete => "I".blue(),
        TestStatus::Risky => "R".magenta(),
    };
    print!("{mark}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

pub fn print_summary(report: &Report) {
    println!();
    println!();
    for outcome in &report.outcomes {
        let (label, color) = match outcome.status {
            TestStatus::Pass => continue,
            TestStatus::Fail => ("FAIL", "red"),
            TestStatus::Error => ("ERROR", "yellow"),
            TestStatus::Skipped => ("SKIP", "cyan"),
            TestStatus::Incomplete => ("INCOMPLETE", "blue"),
            TestStatus::Risky => ("RISKY", "magenta"),
        };
        let colored_label = match color {
            "red" => label.red().bold(),
            "yellow" => label.yellow().bold(),
            "cyan" => label.cyan().bold(),
            "blue" => label.blue().bold(),
            "magenta" => label.magenta().bold(),
            _ => label.normal(),
        };
        let name = match &outcome.dataset {
            Some(ds) => format!("{}::{} ({})", outcome.class, outcome.method, ds),
            None => format!("{}::{}", outcome.class, outcome.method),
        };
        println!("{colored_label}  {name}");
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

    let total = report.outcomes.len();
    let summary = format!(
        "Tests: {total} total, {} passed, {} failed, {} errored, {} skipped, {} incomplete, {} risky ({:.1}ms)",
        report.passed(),
        report.failed(),
        report.errored(),
        report.skipped(),
        report.incomplete(),
        report.risky(),
        report.total_duration_ms,
    );
    if report.is_success() {
        println!("{}", summary.green().bold());
    } else {
        println!("{}", summary.red().bold());
    }
}
