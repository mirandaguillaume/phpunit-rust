//! Extended PCov format: per-test attribution preserved.
//!
//! Shape: `{file: {line: ["Class::method", ...]}}`.
//! Strict PCov consumers that read only keys see covered lines.
//! Aware consumers see which tests covered each line.

use crate::analyzer::Coverage;
use serde_json::{Map, Value};

pub fn render(coverage: &Coverage) -> String {
    let mut out = Map::new();
    for (file, lines) in coverage {
        let mut line_map = Map::new();
        for (line, tests) in lines {
            let ids: Vec<Value> = tests.iter().map(|t| Value::from(t.display())).collect();
            line_map.insert(line.to_string(), Value::Array(ids));
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
    fn renders_per_test_attribution() {
        let mut cov: Coverage = HashMap::new();
        let mut lines = HashMap::new();
        lines.insert(47, vec![
            TestId { class: "T".into(), method: "testA".into(), data_set: Some("0".into()) },
            TestId { class: "T".into(), method: "testA".into(), data_set: Some("1".into()) },
        ]);
        cov.insert(PathBuf::from("src/U.php"), lines);

        let s = render(&cov);
        let parsed: Value = serde_json::from_str(&s).unwrap();
        let ids = parsed["src/U.php"]["47"].as_array().unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], Value::from("T::testA#0"));
        assert_eq!(ids[1], Value::from("T::testA#1"));
    }

    #[test]
    fn renders_empty_line_for_uncovered() {
        let mut cov: Coverage = HashMap::new();
        let mut lines = HashMap::new();
        lines.insert(50, vec![]);
        cov.insert(PathBuf::from("src/V.php"), lines);

        let s = render(&cov);
        let parsed: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["src/V.php"]["50"], Value::Array(vec![]));
    }
}
