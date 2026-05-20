//! Raw internal JSON output.
//!
//! Serializes the Coverage map directly via serde_json. Includes per-test
//! attribution as TestId structs. Useful for tooling that wants the richest
//! representation of the analysis result.

use crate::analyzer::Coverage;

pub fn render(coverage: &Coverage) -> String {
    serde_json::to_string_pretty(coverage).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::TestId;
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[test]
    fn renders_empty_coverage_as_empty_object() {
        let cov: Coverage = HashMap::new();
        let s = render(&cov);
        assert_eq!(s.trim(), "{}");
    }

    #[test]
    fn renders_coverage_with_test_ids() {
        let mut cov: Coverage = HashMap::new();
        let mut lines = HashMap::new();
        lines.insert(10u32, vec![TestId {
            class: "T".into(),
            method: "testA".into(),
            data_set: None,
        }]);
        cov.insert(PathBuf::from("a.php"), lines);

        let s = render(&cov);
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        // The structure has PathBuf as key (serialized as string), then HashMap<u32, Vec<TestId>>.
        assert!(parsed.get("a.php").is_some(), "expected a.php key, got: {parsed}");
    }
}
