//! Join generated mutants with the per-test coverage map: each mutant learns the
//! test ids that execute its line, so the executor re-runs only those. A mutant no
//! test covers keeps an empty list and is reported NotCovered (never dropped).
use crate::mutate::coverage::PerTestCoverage;
use crate::mutate::Mutant;

/// A mutant paired with the tests that must be re-run to try to kill it.
pub struct PlannedMutant {
    pub mutant: Mutant,
    pub covering_tests: Vec<String>,
}

/// Attach each mutant's covering test ids (by file + 1-based line).
pub fn plan(mutants: Vec<Mutant>, cov: &PerTestCoverage) -> Vec<PlannedMutant> {
    mutants
        .into_iter()
        .map(|m| {
            let covering_tests = cov.covering_tests(&m.file, m.line).to_vec();
            PlannedMutant {
                mutant: m,
                covering_tests,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutate::coverage::PerTestCoverage;
    use crate::mutate::Mutant;
    use std::path::PathBuf;

    fn mutant(line: u32) -> Mutant {
        Mutant {
            file: PathBuf::from("/src/Calc.php"),
            start: 0,
            end: 1,
            replacement: b"-".to_vec(),
            mutator: "Plus",
            line,
            report_line: line,
        }
    }

    #[test]
    fn covered_mutant_gets_its_tests_uncovered_gets_none() {
        let cov = PerTestCoverage::from_json(
            br#"{"coverage":{"/src/Calc.php":{"2":["CalcTest::testAdd"]}}}"#,
        )
        .unwrap();
        let out = plan(vec![mutant(2), mutant(9)], &cov);
        assert_eq!(out[0].covering_tests, ["CalcTest::testAdd"]);
        assert!(out[1].covering_tests.is_empty());
    }
}
