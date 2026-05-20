//! Bridge between a completed test Report and pcov-rs coverage analysis.

#[cfg(feature = "coverage")]
mod inner {
    use crate::runner::Report;
    use crate::types::TestStatus;
    use analyzer::{analyze_filtered, parse_config, render, Format};
    use std::collections::HashSet;
    use std::path::Path;

    /// Build the set of (class, method) pairs that passed.
    pub fn passed_set(report: &Report) -> HashSet<(String, String)> {
        report
            .outcomes
            .iter()
            .filter(|o| o.status == TestStatus::Pass)
            .map(|o| (o.class.clone(), o.method.clone()))
            .collect()
    }

    /// Run pcov-rs coverage for passing tests and write output.
    ///
    /// `config_path` — path to phpunit.xml (same one phpunit-rust used).
    /// `allowed`     — set of (class, method) pairs that passed; `None` = all tests.
    /// `format`      — output format string ("clover", "json", "pcov", "pcov-extended").
    /// `out`         — output file path; `None` = write to stdout.
    pub fn emit(
        config_path: &Path,
        allowed: Option<&HashSet<(String, String)>>,
        format: &str,
        out: Option<&Path>,
    ) -> anyhow::Result<()> {
        use std::str::FromStr;
        let fmt = Format::from_str(format)
            .map_err(|e| anyhow::anyhow!("unknown coverage format: {e}"))?;
        let cfg = parse_config(config_path)?;
        let coverage = analyze_filtered(&cfg, allowed)?;
        let rendered = render(fmt, &coverage);
        match out {
            Some(path) => std::fs::write(path, &rendered)?,
            None => print!("{rendered}"),
        }
        Ok(())
    }
}

#[cfg(feature = "coverage")]
pub use inner::{emit, passed_set};

#[cfg(test)]
mod tests {
    #[cfg(feature = "coverage")]
    #[test]
    fn passed_set_excludes_failures() {
        use crate::runner::Report;
        use crate::types::{TestOutcome, TestStatus};
        let report = Report {
            outcomes: vec![
                TestOutcome {
                    class: "A".into(), method: "testOk".into(), dataset: None,
                    status: TestStatus::Pass, message: None, trace: None,
                    duration_ms: 1.0,
                },
                TestOutcome {
                    class: "A".into(), method: "testFail".into(), dataset: None,
                    status: TestStatus::Fail, message: None, trace: None,
                    duration_ms: 1.0,
                },
            ],
            total_duration_ms: 2.0,
        };
        let set = super::inner::passed_set(&report);
        assert!(set.contains(&("A".into(), "testOk".into())));
        assert!(!set.contains(&("A".into(), "testFail".into())));
    }
}
