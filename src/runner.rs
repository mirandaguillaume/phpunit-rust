use crate::client::WorkerClient;
use crate::discovery::{group_by_class, TestClass};
use crate::frankenphp::WorkerPool;
use crate::types::{TestCase, TestOutcome, TestRunRequest, TestStatus};
use anyhow::Result;
use rayon::prelude::*;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub autoload: PathBuf,
    pub bootstrap: Option<PathBuf>,
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
    pool: &WorkerPool,
    cases: Vec<TestCase>,
    cfg: &RunConfig,
    on_progress: impl Fn(&TestOutcome) + Sync,
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

    // One WorkerClient per pool worker. Cloned ureq::Agent shares connection
    // pool internals; per-worker we want distinct URLs and isolated state.
    let urls = pool.urls();
    let clients: Vec<WorkerClient> =
        urls.iter().map(|u| WorkerClient::new(u.clone())).collect();

    // Distribute classes via rayon. Each rayon thread picks its client by
    // thread index; rayon guarantees that index < pool size when the global
    // thread pool was sized to match (see main.rs).
    let results: Vec<Result<Vec<TestOutcome>>> = groups
        .into_par_iter()
        .map(|TestClass { file, class, methods }| {
            let idx = rayon::current_thread_index().unwrap_or(0);
            let client = &clients[idx % clients.len()];
            let req = TestRunRequest {
                autoload: cfg.autoload.clone(),
                bootstrap: cfg.bootstrap.clone(),
                file,
                class,
                methods,
            };
            let batch = client.run_class(&req)?;
            // Emit progress inside the worker thread, as outcomes arrive.
            for outcome in &batch {
                on_progress(outcome);
            }
            Ok(batch)
        })
        .collect();

    // Aggregate. Short-circuit on first transport/protocol error so the
    // user sees the actual cause; per-test failures are returned in
    // outcomes, not as Result errors.
    let mut outcomes = Vec::new();
    let mut total = 0.0;
    for batch in results {
        let batch = batch?;
        for outcome in batch {
            total += outcome.duration_ms;
            outcomes.push(outcome);
        }
    }
    Ok(Report { outcomes, total_duration_ms: total })
}
