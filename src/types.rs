use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Slice a method's data-provider rows by stride. `chunk_index` is in
/// `0..total_chunks`; the worker keeps rows whose 0-based position
/// satisfies `pos % total_chunks == chunk_index`. Stride splitting (vs
/// range splitting) balances workloads when the provider returns rows
/// in increasing-difficulty order, which is common.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowFilter {
    pub chunk_index: usize,
    pub total_chunks: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCase {
    pub file: PathBuf,
    pub class: String,
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TestRunRequest {
    pub autoload: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<PathBuf>,
    pub file: PathBuf,
    pub class: String,
    /// Empty vec means "run all test methods in the class".
    pub methods: Vec<String>,
    /// PHP `define()` declarations from `<php><const .../>` blocks in
    /// phpunit.xml. Worker applies these once per autoload before any
    /// test runs. Omitted from JSON when empty (avoid noise on tiny
    /// fixtures that have no phpunit.xml at all).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub defines: Vec<[String; 2]>,
    /// When true, the worker returns a `ClassDescriptor` (method + depends
    /// info) instead of running anything. Used by the runner's probe phase
    /// to decide how to split a class across workers.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub describe_only: bool,
    /// Optional row-stride filter applied to the (single) method named in
    /// `methods[0]`. None = no filter (run all rows). Only meaningful when
    /// `methods` has exactly one entry; otherwise behavior is undefined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_filter: Option<RowFilter>,
}

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

/// One method as reported by the worker's describe mode.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MethodDescriptor {
    pub name: String,
    #[serde(default)]
    pub depends: Vec<String>,
    /// Number of rows the data provider returns. `None` for non-parameterized
    /// methods or when the provider couldn't be counted (we don't crash the
    /// probe on provider errors — they surface at run time).
    #[serde(default)]
    pub row_count: Option<usize>,
}

/// Worker's response to a `describe_only=true` request.
#[derive(Debug, Clone, Deserialize)]
pub struct ClassDescriptor {
    pub description: Vec<MethodDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BatchClass {
    pub file: PathBuf,
    pub class: String,
    /// Empty = all discovered methods. Worker calls MethodPlanner::plan()
    /// internally for @depends ordering.
    pub methods: Vec<String>,
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
    fn run_request_omits_bootstrap_when_none() {
        let req = TestRunRequest {
            autoload: PathBuf::from("/p/vendor/autoload.php"),
            bootstrap: None,
            file: PathBuf::from("/p/tests/Foo.php"),
            class: "App\\Tests\\FooTest".into(),
            methods: vec![],
            defines: vec![],
            describe_only: false,
            row_filter: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("bootstrap").is_none());
        assert!(json.get("defines").is_none(), "defines must be omitted when empty");
        assert_eq!(json["class"], "App\\Tests\\FooTest");
    }

    #[test]
    fn run_request_includes_bootstrap_when_present() {
        let req = TestRunRequest {
            autoload: PathBuf::from("/p/vendor/autoload.php"),
            bootstrap: Some(PathBuf::from("/p/phpunit.php")),
            file: PathBuf::from("/p/tests/Foo.php"),
            class: "FooTest".into(),
            methods: vec!["testBar".into()],
            defines: vec![["API_KEY".into(), "xyz".into()]],
            describe_only: false,
            row_filter: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["bootstrap"], "/p/phpunit.php");
        assert_eq!(json["defines"][0][0], "API_KEY");
        assert_eq!(json["defines"][0][1], "xyz");
        assert_eq!(json["methods"][0], "testBar");
    }

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
                file: PathBuf::from("/proj/tests/FooTest.php"),
                class: "App\\FooTest".to_string(),
                methods: vec!["testA".to_string()],
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
