use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
