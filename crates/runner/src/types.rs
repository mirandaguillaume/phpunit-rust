use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// TestCase moved to the shared `discovery` crate (so the analyzer can
// consume it too). Re-exported here for the historical import path.
pub use discovery::TestCase;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TestStatus {
    Pass,
    Fail,
    Error,
    Skipped,
    Incomplete,
    Risky,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TestOutcome {
    pub class: String,
    pub method: String,
    /// PHPUnit data-provider row identifier, e.g. "0" or "with strings".
    /// `None` for tests that aren't parameterized.
    #[serde(default)]
    pub dataset: Option<String>,
    pub status: TestStatus,
    pub message: Option<String>,
    pub trace: Option<String>,
    pub duration_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RowFilter {
    pub chunk_index:  u32,
    pub total_chunks: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BatchClass {
    pub file: PathBuf,
    pub class: String,
    /// Empty = all discovered methods. Worker calls MethodPlanner::plan()
    /// internally for @depends ordering.
    pub methods: Vec<String>,
    /// Optional row filter applied to all data-provider methods in this
    /// batch (stride partition: keep row i iff i % total_chunks == chunk_index).
    /// `None` = no filter (run every row). Used by the runner to split a
    /// fat data-provider method across multiple workers — the heavy method
    /// becomes N BatchClass entries with different chunk_index values.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_filter: Option<RowFilter>,
    /// Extra PHP files to `require_once` before running this class.
    /// Used for `#[DataProviderExternal]` provider classes not in the
    /// PSR-4 autoloader.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub required_files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchPlan {
    pub autoload: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<PathBuf>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub defines: Vec<[String; 2]>,
    pub classes: Vec<BatchClass>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_deserializes_with_dataset() {
        let raw = r#"{"class":"A","method":"b","dataset":"with strings","status":"pass","message":null,"trace":null,"duration_ms":1.0}"#;
        let outcome: TestOutcome = serde_json::from_str(raw).unwrap();
        assert_eq!(outcome.dataset.as_deref(), Some("with strings"));
        assert_eq!(outcome.status, TestStatus::Pass);
    }

    #[test]
    fn outcome_deserializes_without_dataset() {
        let raw = r#"{"class":"A","method":"b","status":"skipped","message":"reason","trace":null,"duration_ms":0.0}"#;
        let outcome: TestOutcome = serde_json::from_str(raw).unwrap();
        assert!(outcome.dataset.is_none());
        assert_eq!(outcome.status, TestStatus::Skipped);
    }

    #[test]
    fn outcome_deserializes_all_new_statuses() {
        for (raw_status, expected) in [
            ("pass", TestStatus::Pass),
            ("fail", TestStatus::Fail),
            ("error", TestStatus::Error),
            ("skipped", TestStatus::Skipped),
            ("incomplete", TestStatus::Incomplete),
            ("risky", TestStatus::Risky),
        ] {
            let raw = format!(r#"{{"class":"A","method":"b","status":"{}","message":null,"trace":null,"duration_ms":0.0}}"#, raw_status);
            let outcome: TestOutcome = serde_json::from_str(&raw).unwrap();
            assert_eq!(outcome.status, expected);
        }
    }
}

#[cfg(test)]
mod batch_plan_tests {
    use super::*;

    #[test]
    fn batch_plan_serializes_correctly() {
        let plan = BatchPlan {
            autoload: PathBuf::from("/proj/vendor/autoload.php"),
            bootstrap: Some(PathBuf::from("/proj/bootstrap.php")),
            defines: vec![["FOO".to_string(), "bar".to_string()]],
            classes: vec![BatchClass {
                file:           PathBuf::from("/proj/tests/FooTest.php"),
                class:          "App\\FooTest".to_string(),
                methods:        vec!["testA".to_string()],
                row_filter:     None,
                required_files: vec![],
            }],
        };
        let v = serde_json::to_value(&plan).unwrap();
        assert_eq!(v["autoload"], "/proj/vendor/autoload.php");
        assert_eq!(v["bootstrap"], "/proj/bootstrap.php");
        assert_eq!(v["defines"][0][0], "FOO");
        assert_eq!(v["classes"][0]["class"], "App\\FooTest");
        assert_eq!(v["classes"][0]["methods"][0], "testA");
    }

    #[test]
    fn batch_class_required_files_omitted_when_empty() {
        let bc = BatchClass {
            file:           PathBuf::from("/t/FooTest.php"),
            class:          "FooTest".to_string(),
            methods:        vec![],
            row_filter:     None,
            required_files: vec![],
        };
        let v = serde_json::to_value(&bc).unwrap();
        assert!(v.get("required_files").is_none());
    }

    #[test]
    fn batch_class_required_files_present_when_non_empty() {
        let bc = BatchClass {
            file:           PathBuf::from("/t/FooTest.php"),
            class:          "FooTest".to_string(),
            methods:        vec![],
            row_filter:     None,
            required_files: vec![PathBuf::from("/t/Provider.php")],
        };
        let v = serde_json::to_value(&bc).unwrap();
        assert_eq!(v["required_files"][0], "/t/Provider.php");
    }

    #[test]
    fn batch_plan_omits_bootstrap_when_none() {
        let plan = BatchPlan {
            autoload: PathBuf::from("/p/vendor/autoload.php"),
            bootstrap: None,
            defines: vec![],
            classes: vec![],
        };
        let v = serde_json::to_value(&plan).unwrap();
        assert!(v.get("bootstrap").is_none());
        assert!(v.get("defines").is_none(), "empty defines must be omitted");
    }
}
