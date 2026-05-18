use crate::client::WorkerClient;
use crate::discovery::{group_by_class, TestClass};
use crate::types::{TestCase, TestOutcome, TestRunRequest, TestStatus};
use anyhow::Result;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub autoload: PathBuf,
    pub phpunit_xml: Option<PathBuf>,
    pub filter: Option<String>,
}

#[derive(Debug)]
pub struct Report {
    pub outcomes: Vec<TestOutcome>,
    pub total_duration_ms: f64,
}

impl Report {
    pub fn count(&self, status: TestStatus) -> usize {
        self.outcomes.iter().filter(|o| o.status == status).count()
    }
    pub fn passed(&self) -> usize { self.count(TestStatus::Pass) }
    pub fn failed(&self) -> usize { self.count(TestStatus::Fail) }
    pub fn errored(&self) -> usize { self.count(TestStatus::Error) }
    pub fn skipped(&self) -> usize { self.count(TestStatus::Skipped) }
    pub fn incomplete(&self) -> usize { self.count(TestStatus::Incomplete) }
    pub fn risky(&self) -> usize { self.count(TestStatus::Risky) }

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
    // Apply class-level filter pre-batch (so we don't ship classes that have
    // no matching methods). Inside a class, the worker filters by methods.
    let filtered_cases: Vec<TestCase> = cases
        .into_iter()
        .filter(|c| match &cfg.filter {
            Some(f) => format!("{}::{}", c.class, c.method).contains(f),
            None => true,
        })
        .collect();

    let groups = group_by_class(filtered_cases);

    let mut outcomes = Vec::new();
    let mut total = 0.0;
    for TestClass { file, class, methods } in groups {
        let req = TestRunRequest {
            autoload: cfg.autoload.clone(),
            phpunit_xml: cfg.phpunit_xml.clone(),
            file,
            class,
            methods,
        };
        let batch = client.run_class(&req)?;
        for outcome in batch {
            total += outcome.duration_ms;
            on_progress(&outcome);
            outcomes.push(outcome);
        }
    }
    Ok(Report { outcomes, total_duration_ms: total })
}
