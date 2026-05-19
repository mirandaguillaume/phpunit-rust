use crate::components;
use crate::discovery::{group_by_class, TestClass};
use crate::php_client::PhpWorkerClient;
use crate::php_worker::PhpWorkerPool;
use crate::types::{ClassDescriptor, RowFilter, TestCase, TestOutcome, TestRunRequest, TestStatus};
use anyhow::Result;
use rayon::prelude::*;
use std::collections::HashMap;
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
    /// Minimum row count for a data-provider method to be split into per-row
    /// chunks. Below this, methods are dispatched whole. Default 50.
    pub row_chunk_min: usize,
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
    pool: &PhpWorkerPool,
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

    let total_workers = pool.len();

    // Phase A: probe each class for its depends info and row counts.
    // Sequential — one round-trip per class, cheap.
    // All probes go to worker 0; no contention.
    let probe_client = PhpWorkerClient::new(pool.worker(0));

    // Per-component dispatch units; each unit becomes ONE round-trip.
    // (file, class, methods_subset, row_filter)
    let mut dispatch_units: Vec<(PathBuf, String, Vec<String>, Option<RowFilter>)> = Vec::new();

    // Error outcomes from classes whose describe probe failed (PHP OOM, etc.).
    // Collected here and merged with Phase-B outcomes at the end.
    let mut describe_errors: Vec<TestOutcome> = Vec::new();

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
        let descriptor: ClassDescriptor = match probe_client.describe_class(&probe_req) {
            Ok(d) => d,
            Err(e) => {
                // Worker crashed (OOM, fatal error) — can't know method list.
                // Use the methods we discovered statically as best-effort.
                let msg = format!("{e:#}");
                for method in methods {
                    describe_errors.push(TestOutcome {
                        class: class.clone(),
                        method: method.clone(),
                        dataset: None,
                        status: TestStatus::Error,
                        message: Some(msg.clone()),
                        trace: None,
                        duration_ms: 0.0,
                    });
                }
                continue;
            }
        };

        // Build the depends map AND row-count map from the descriptor.
        let depends: HashMap<String, Vec<String>> = descriptor
            .description
            .iter()
            .map(|m| (m.name.clone(), m.depends.clone()))
            .collect();
        let row_counts: HashMap<String, Option<usize>> = descriptor
            .description
            .iter()
            .map(|m| (m.name.clone(), m.row_count))
            .collect();

        // Restrict to the methods we discovered (which respects any --filter
        // we already applied at the case level).
        let class_methods: Vec<String> = methods.clone();
        let components = components::partition_by_depends(&class_methods, &depends);

        for component in components {
            // If the component is a single method with row_count > threshold,
            // split it into `total_workers` row-chunks.
            if component.len() == 1 {
                let m = &component[0];
                let count = row_counts.get(m).copied().flatten().unwrap_or(0);
                if count > cfg.row_chunk_min && total_workers > 1 {
                    for chunk_index in 0..total_workers {
                        dispatch_units.push((
                            file.clone(),
                            class.clone(),
                            vec![m.clone()],
                            Some(RowFilter { chunk_index, total_chunks: total_workers }),
                        ));
                    }
                    continue;
                }
            }
            // Default: dispatch the whole component as one unit, no row_filter.
            dispatch_units.push((file.clone(), class.clone(), component, None));
        }
    }

    // Phase B: parallel dispatch via rayon.
    // Each rayon thread picks a worker by its thread index.
    let results: Vec<Result<Vec<TestOutcome>>> = dispatch_units
        .into_par_iter()
        .map(|(file, class, methods, row_filter)| {
            let idx = rayon::current_thread_index().unwrap_or(0);
            let client = PhpWorkerClient::new(pool.worker(idx));
            let req = TestRunRequest {
                autoload: cfg.autoload.clone(),
                bootstrap: cfg.bootstrap.clone(),
                file,
                class,
                methods,
                defines: cfg.defines.clone(),
                describe_only: false,
                row_filter,
            };
            let batch = client.run_class(&req)?;
            // Emit progress inside the worker thread as outcomes arrive.
            for outcome in &batch {
                on_progress(outcome);
            }
            Ok(batch)
        })
        .collect();

    // Aggregate. Short-circuit on the first transport error (process died).
    // Worker-level class errors were already converted to error outcomes inside
    // run_class, so they arrive as Ok(outcomes) and are counted normally.
    let mut outcomes: Vec<TestOutcome> = describe_errors;
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
