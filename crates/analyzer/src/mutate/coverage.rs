//! Per-test coverage: the `file -> line -> [covering test ids]` map that drives
//! which tests a mutant must re-run. It is produced by `php/pertest_coverage.php`
//! (which reads the delegated `.cov` files from #99 — they already retain per-test
//! data because `TestExecutor` brackets each test with `$coverage->start($testId)`).
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct Raw {
    coverage: HashMap<String, HashMap<String, Vec<String>>>,
}

/// `file -> line(1-based) -> covering proust test ids` (`Class::method[#dataset]`).
pub struct PerTestCoverage(pub HashMap<PathBuf, HashMap<u32, Vec<String>>>);

impl PerTestCoverage {
    /// Parse the JSON emitted by `php/pertest_coverage.php`.
    pub fn from_json(bytes: &[u8]) -> anyhow::Result<Self> {
        let raw: Raw = serde_json::from_slice(bytes)?;
        let mut m = HashMap::new();
        for (file, lines) in raw.coverage {
            let inner = lines
                .into_iter()
                .filter_map(|(l, ids)| l.parse::<u32>().ok().map(|l| (l, ids)))
                .collect();
            m.insert(PathBuf::from(file), inner);
        }
        Ok(Self(m))
    }

    /// The tests covering `line` of `file` (empty when none — the mutant is NotCovered).
    pub fn covering_tests(&self, file: &Path, line: u32) -> &[String] {
        self.0
            .get(file)
            .and_then(|lines| lines.get(&line))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_looks_up() {
        let json = br#"{"coverage":{"/src/Calc.php":{"2":["CalcTest::testAdd"]}}}"#;
        let c = PerTestCoverage::from_json(json).unwrap();
        assert_eq!(
            c.covering_tests(Path::new("/src/Calc.php"), 2),
            ["CalcTest::testAdd"]
        );
        assert!(c.covering_tests(Path::new("/src/Calc.php"), 99).is_empty());
        assert!(c.covering_tests(Path::new("/nope.php"), 2).is_empty());
    }
}
