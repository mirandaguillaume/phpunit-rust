//! RPC over stdio against a PhpWorker. Same shape as the old WorkerClient
//! (run_class + describe_class) but the transport is `Stdio::piped()` JSON
//! lines instead of ureq HTTP.

use crate::php_worker::PhpWorker;
use crate::types::{ClassDescriptor, TestOutcome, TestRunRequest};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

#[derive(Deserialize)]
struct WorkerResponse {
    outcomes: Vec<TestOutcome>,
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

    pub fn run_class(&self, req: &TestRunRequest) -> Result<Vec<TestOutcome>> {
        let line = self.raw_round_trip(req)?;
        // Worker can return either {outcomes:[...]} or {error:"..."}.
        if line.contains("\"error\"") && !line.contains("\"outcomes\"") {
            return Err(anyhow!("worker error: {}", line.trim()));
        }
        let body: WorkerResponse = serde_json::from_str(&line)
            .with_context(|| format!("worker response was not valid JSON: {}", line.trim()))?;
        Ok(body.outcomes)
    }

    pub fn describe_class(&self, req: &TestRunRequest) -> Result<ClassDescriptor> {
        let line = self.raw_round_trip(req)?;
        if line.contains("\"error\"") && !line.contains("\"description\"") {
            return Err(anyhow!("worker error: {}", line.trim()));
        }
        let body: ClassDescriptor = serde_json::from_str(&line)
            .with_context(|| format!("describe response was not valid JSON: {}", line.trim()))?;
        Ok(body)
    }
}
