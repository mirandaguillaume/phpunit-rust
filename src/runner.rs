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
    /// `define(name, value)` declarations extracted from `<php><const .../>`
    /// in phpunit.xml. Passed through every request so the worker applies
    /// them once per autoload before running tests.
    pub defines: Vec<[String; 2]>,
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
    if groups.is_empty() {
        return Ok(Report { outcomes: Vec::new(), total_duration_ms: 0.0 });
    }

    // One WorkerClient per pool worker.
    let urls = pool.urls();
    let clients: Vec<WorkerClient> =
        urls.iter().map(|u| WorkerClient::new(u.clone())).collect();

    // Phase A: probe each class for its depends info, then split into
    // dependency components. Sequential — one round-trip per class, cheap.
    // We dispatch all probes to clients[0]; parallelizing the probes saves
    // negligible time and adds complication.
    let probe_client = &clients[0];
    let mut dispatch_units: Vec<(PathBuf, String, Vec<String>)> = Vec::new();
    for TestClass { file, class, methods } in &groups {
        let probe_req = TestRunRequest {
            autoload: cfg.autoload.clone(),
            bootstrap: cfg.bootstrap.clone(),
            file: file.clone(),
            class: class.clone(),
            methods: Vec::new(),
            defines: cfg.defines.clone(),
            describe_only: true,
            row_filter: None,
        };
        let descriptor = probe_client.describe_class(&probe_req)?;
        // Build the depends map from the descriptor.
        let depends: std::collections::HashMap<String, Vec<String>> = descriptor
            .description
            .iter()
            .map(|m| (m.name.clone(), m.depends.clone()))
            .collect();
        // Restrict to the methods we discovered (which respects any --filter
        // we already applied at the case level).
        let class_methods: Vec<String> = methods.clone();
        let components = crate::components::partition_by_depends(&class_methods, &depends);
        for component in components {
            dispatch_units.push((file.clone(), class.clone(), component));
        }
    }

    // Phase B: parallel dispatch. Each rayon thread picks a worker by index.
    let results: Vec<Result<Vec<TestOutcome>>> = dispatch_units
        .into_par_iter()
        .map(|(file, class, methods)| {
            let idx = rayon::current_thread_index().unwrap_or(0);
            let client = &clients[idx % clients.len()];
            let req = TestRunRequest {
                autoload: cfg.autoload.clone(),
                bootstrap: cfg.bootstrap.clone(),
                file,
                class,
                methods,
                defines: cfg.defines.clone(),
                describe_only: false,
                row_filter: None,
            };
            let batch = client.run_class(&req)?;
            // Emit progress inside the worker thread as outcomes arrive.
            for outcome in &batch {
                on_progress(outcome);
            }
            Ok(batch)
        })
        .collect();

    // Aggregate. Short-circuit on the first transport error.
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
