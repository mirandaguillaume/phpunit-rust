//! RPC over stdio against a PhpWorker. Same shape as the old WorkerClient
//! (run_class + describe_class) but the transport is `Stdio::piped()` JSON
//! lines instead of ureq HTTP.

use crate::php_worker::PhpWorker;
use crate::types::{ClassDescriptor, TestOutcome, TestRunRequest, TestStatus};
use anyhow::{Context, Result};
use serde::Deserialize;

/// Worker can send back either successful outcomes or a class-level error.
/// `#[serde(untagged)]` tries Success first (requires `outcomes` key), then
/// falls back to Failure for any JSON object that lacks `outcomes`.
#[derive(Deserialize)]
#[serde(untagged)]
enum WorkerRunReply {
    Success { outcomes: Vec<TestOutcome> },
    Failure { detail: Option<String>, trace: Option<String> },
}

/// Borrowed handle: holds a reference to one worker in the pool. The pool
/// itself owns workers; clients are stateless and cheap to construct.
pub struct PhpWorkerClient<'a> {
    worker: &'a PhpWorker,
}

impl<'a> PhpWorkerClient<'a> {
    pub fn new(worker: &'a PhpWorker) -> Self {
        Self { worker }
    }

    /// Send the request as a JSON line, read one JSON response line.
    /// Used by both run_class and describe_class; deserialization differs.
    fn raw_round_trip(&self, req: &TestRunRequest) -> Result<String> {
        let json = serde_json::to_string(req).context("serializing request")?;
        let response = self.worker.round_trip(&json)?;
        Ok(response)
    }

    /// Run a class on the worker. Worker errors (exceptions in setUp/tearDown,
    /// etc.) are converted to error-status TestOutcomes instead of propagating
    /// as Err — the run continues for all other classes.
    pub fn run_class(&self, req: &TestRunRequest) -> Result<Vec<TestOutcome>> {
        let line = self.raw_round_trip(req)?;
        match serde_json::from_str::<WorkerRunReply>(&line)
            .with_context(|| format!("worker response was not valid JSON: {}", line.trim()))?
        {
            WorkerRunReply::Success { outcomes } => Ok(outcomes),
            WorkerRunReply::Failure { detail, trace } => {
                let msg = detail.unwrap_or_else(|| "worker error (no detail)".into());
                let targets = if req.methods.is_empty() {
                    vec!["<class>".to_string()]
                } else {
                    req.methods.clone()
                };
                Ok(targets.into_iter().map(|m| TestOutcome {
                    class: req.class.clone(),
                    method: m,
                    dataset: None,
                    status: TestStatus::Error,
                    message: Some(msg.clone()),
                    trace: trace.clone(),
                    duration_ms: 0.0,
                }).collect())
            }
        }
    }

    pub fn describe_class(&self, req: &TestRunRequest) -> Result<ClassDescriptor> {
        let line = self.raw_round_trip(req)?;
        let body: ClassDescriptor = serde_json::from_str(&line)
            .with_context(|| format!("describe response was not valid JSON: {}", line.trim()))?;
        Ok(body)
    }
}
