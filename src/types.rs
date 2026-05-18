use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCase {
    pub file: PathBuf,
    pub class: String,
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TestRequest {
    pub autoload: PathBuf,
    pub file: PathBuf,
    pub class: String,
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TestStatus {
    Pass,
    Fail,
    Error,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TestOutcome {
    pub class: String,
    pub method: String,
    pub status: TestStatus,
    pub message: Option<String>,
    pub trace: Option<String>,
    pub duration_ms: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serializes_with_expected_keys() {
        let req = TestRequest {
            autoload: PathBuf::from("/p/vendor/autoload.php"),
            file: PathBuf::from("/p/tests/Foo.php"),
            class: "App\\Tests\\FooTest".into(),
            method: "testBar".into(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["autoload"], "/p/vendor/autoload.php");
        assert_eq!(json["class"], "App\\Tests\\FooTest");
        assert_eq!(json["method"], "testBar");
    }

    #[test]
    fn test_outcome_deserializes_pass() {
        let raw = r#"{"class":"A","method":"b","status":"pass","message":null,"trace":null,"duration_ms":1.5}"#;
        let outcome: TestOutcome = serde_json::from_str(raw).unwrap();
        assert_eq!(outcome.status, TestStatus::Pass);
        assert_eq!(outcome.duration_ms, 1.5);
    }

    #[test]
    fn test_outcome_deserializes_fail_with_message() {
        let raw = "{\"class\":\"A\",\"method\":\"b\",\"status\":\"fail\",\"message\":\"oops\",\"trace\":\"#0 ...\",\"duration_ms\":0.3}";
        let outcome: TestOutcome = serde_json::from_str(raw).unwrap();
        assert_eq!(outcome.status, TestStatus::Fail);
        assert_eq!(outcome.message.as_deref(), Some("oops"));
    }
}
