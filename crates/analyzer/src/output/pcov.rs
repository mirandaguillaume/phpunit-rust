//! Strict PCov format: aggregate hit-state per line.
//!
//! Matches `\pcov\collect()`'s schema: `{file: {line: 1|-1}}` where `1`
//! means "covered by at least one test" and `-1` means "executable but
//! not covered". Per-test attribution is discarded.

use crate::analyzer::Coverage;
use serde_json::Value;

pub fn render(coverage: &Coverage) -> String {
    let mut out = serde_json::Map::new();
    for (file, lines) in coverage {
        let mut line_map = serde_json::Map::new();
        for (line, tests) in lines {
            let hit: i64 = if tests.is_empty() { -1 } else { 1 };
            line_map.insert(line.to_string(), Value::from(hit));
        }
        out.insert(file.display().to_string(), Value::Object(line_map));
    }
    serde_json::to_string_pretty(&Value::Object(out)).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::TestId;
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[test]
    fn renders_covered_and_uncovered() {
        let mut cov: Coverage = HashMap::new();
        let mut lines = HashMap::new();
        lines.insert(47, vec![TestId { class: "T".into(), method: "testA".into(), data_set: None }]);
        lines.insert(48, vec![]);
        cov.insert(PathBuf::from("src/U.php"), lines);

        let s = render(&cov);
        let parsed: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["src/U.php"]["47"], Value::from(1));
        assert_eq!(parsed["src/U.php"]["48"], Value::from(-1));
    }

    #[test]
    fn renders_empty_coverage() {
        let cov: Coverage = HashMap::new();
        let s = render(&cov);
        let parsed: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, Value::Object(serde_json::Map::new()));
    }
}
