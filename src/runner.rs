use crate::client::WorkerClient;
use crate::types::{TestCase, TestOutcome, TestRequest};
use anyhow::Result;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub autoload: PathBuf,
    pub filter: Option<String>,
}

#[derive(Debug)]
pub struct Report {
    pub outcomes: Vec<TestOutcome>,
    pub total_duration_ms: f64,
}

impl Report {
    pub fn passed(&self) -> usize {
        self.outcomes.iter().filter(|o| matches!(o.status, crate::types::TestStatus::Pass)).count()
    }
    pub fn failed(&self) -> usize {
        self.outcomes.iter().filter(|o| matches!(o.status, crate::types::TestStatus::Fail)).count()
    }
    pub fn errored(&self) -> usize {
        self.outcomes.iter().filter(|o| matches!(o.status, crate::types::TestStatus::Error)).count()
    }
    pub fn is_success(&self) -> bool {
        self.failed() == 0 && self.errored() == 0
    }
}

pub fn run(
    client: &WorkerClient,
    cases: Vec<TestCase>,
    cfg: &RunConfig,
    mut on_progress: impl FnMut(&TestOutcome),
) -> Result<Report> {
    let mut outcomes = Vec::new();
    let mut total = 0.0;
    for case in cases {
        if let Some(filter) = &cfg.filter {
            let fqn = format!("{}::{}", case.class, case.method);
            if !fqn.contains(filter) {
                continue;
            }
        }
        let req = TestRequest {
            autoload: cfg.autoload.clone(),
            file: case.file.clone(),
            class: case.class.clone(),
            method: case.method.clone(),
        };
        let outcome = client.run_test(&req)?;
        total += outcome.duration_ms;
        on_progress(&outcome);
        outcomes.push(outcome);
    }
    Ok(Report { outcomes, total_duration_ms: total })
}
