//! File-format test reports (JUnit XML, TestDox) rendered from a finished
//! [`Report`]. Each format is a pure `fn(&Report) -> String` so it can be unit
//! tested in isolation; `main` decides when to write/print based on CLI flags.
//!
//! These consume only the data already carried by [`TestOutcome`] (class,
//! method, dataset, status, message, trace, duration) — no PHP-side or protocol
//! changes. Consequently the JUnit XML omits per-test `assertions` and
//! `file`/`line` attributes (not carried today); it is still valid JUnit and is
//! consumed as-is by GitLab, GitHub, and the Jenkins JUnit plugin.

pub mod junit;
pub mod testdox;

use crate::runner::Report;
use crate::types::TestOutcome;

/// Group outcomes by their declaring class, preserving first-seen order both of
/// classes and of the outcomes within each class. Shared by both formats.
pub(crate) fn group_by_class(report: &Report) -> Vec<(&str, Vec<&TestOutcome>)> {
    let mut index: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut groups: Vec<(&str, Vec<&TestOutcome>)> = Vec::new();
    for outcome in &report.outcomes {
        let class = outcome.class.as_str();
        match index.get(class) {
            Some(&i) => groups[i].1.push(outcome),
            None => {
                index.insert(class, groups.len());
                groups.push((class, vec![outcome]));
            }
        }
    }
    groups
}
