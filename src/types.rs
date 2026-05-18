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
    /// Path to phpunit.xml if the user has one. None → use PHPUnit defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phpunit_xml: Option<PathBuf>,
    pub file: PathBuf,
    pub class: String,
    /// Empty vec means "run all test methods in the class".
    pub methods: Vec<String>,
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
    fn run_request_omits_phpunit_xml_when_none() {
        let req = TestRunRequest {
            autoload: PathBuf::from("/p/vendor/autoload.php"),
            phpunit_xml: None,
            file: PathBuf::from("/p/tests/Foo.php"),
            class: "App\\Tests\\FooTest".into(),
            methods: vec![],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("phpunit_xml").is_none());
        assert_eq!(json["class"], "App\\\\Tests\\\\FooTest");
    }

    #[test]
    fn run_request_includes_phpunit_xml_when_present() {
        let req = TestRunRequest {
            autoload: PathBuf::from("/p/vendor/autoload.php"),
            phpunit_xml: Some(PathBuf::from("/p/phpunit.xml")),
            file: PathBuf::from("/p/tests/Foo.php"),
            class: "FooTest".into(),
            methods: vec!["testBar".into()],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["phpunit_xml"], "/p/phpunit.xml");
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
