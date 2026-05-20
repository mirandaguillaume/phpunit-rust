//! Per-test coverage analyzer.
//!
//! For each TestMethod, walks the method body and records every visited line
//! against a TestId. Dispatch resolution into callees is deferred to a future
//! lexical type tracker; currently all calls are treated as opaque (mark call
//! site, don't recurse).

pub mod trace;
pub mod dispatch;
pub mod data_provider;
pub mod proxy;

pub use data_provider::ExpandedTest;

use std::collections::HashMap;
use std::path::PathBuf;

/// Identifier for a single test invocation, including data provider row.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TestId {
    pub class: String,
    pub method: String,
    pub data_set: Option<String>,
}

impl TestId {
    /// Render to PHPUnit-style display: `Class::method` or `Class::method#dataSet`.
    pub fn display(&self) -> String {
        match &self.data_set {
            Some(d) => format!("{}::{}#{}", self.class, self.method, d),
            None => format!("{}::{}", self.class, self.method),
        }
    }
}

/// Coverage map: file → line → tests that covered that line.
pub type Coverage = HashMap<PathBuf, HashMap<u32, Vec<TestId>>>;

/// Merge `addition` into `into`, appending TestIds at each (file, line).
pub fn merge(into: &mut Coverage, addition: Coverage) {
    for (path, line_map) in addition {
        let target = into.entry(path).or_default();
        for (line, ids) in line_map {
            target.entry(line).or_default().extend(ids);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_id_display_without_data_set() {
        let id = TestId { class: "Foo".into(), method: "testBar".into(), data_set: None };
        assert_eq!(id.display(), "Foo::testBar");
    }

    #[test]
    fn test_id_display_with_data_set() {
        let id = TestId { class: "Foo".into(), method: "testBar".into(), data_set: Some("0".into()) };
        assert_eq!(id.display(), "Foo::testBar#0");
    }

    #[test]
    fn merge_appends_test_ids() {
        let mut a: Coverage = HashMap::new();
        let f = PathBuf::from("a.php");
        let mut a_lines = HashMap::new();
        a_lines.insert(1, vec![TestId { class: "T".into(), method: "testA".into(), data_set: None }]);
        a.insert(f.clone(), a_lines);

        let mut b: Coverage = HashMap::new();
        let mut b_lines = HashMap::new();
        b_lines.insert(1, vec![TestId { class: "T".into(), method: "testB".into(), data_set: None }]);
        b.insert(f.clone(), b_lines);

        merge(&mut a, b);
        let line_1 = a.get(&f).unwrap().get(&1).unwrap();
        assert_eq!(line_1.len(), 2);
    }
}
