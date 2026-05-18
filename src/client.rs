use crate::types::{TestOutcome, TestRunRequest};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize)]
struct WorkerResponse {
    outcomes: Vec<TestOutcome>,
}

pub struct WorkerClient {
    url: String,
    agent: ureq::Agent,
}

impl WorkerClient {
    pub fn new(url: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(60))
            .build();
        Self { url: url.into(), agent }
    }

    pub fn run_class(&self, req: &TestRunRequest) -> Result<Vec<TestOutcome>> {
        let resp = self.agent
            .post(&self.url)
            .set("Content-Type", "application/json")
            .send_json(req)
            .map_err(|e| match e {
                ureq::Error::Status(code, r) => {
                    let body = r.into_string().unwrap_or_default();
                    anyhow!("worker returned HTTP {code}: {body}")
                }
                ureq::Error::Transport(t) => anyhow!("transport error talking to worker: {t}"),
            })?;
        let body: WorkerResponse = resp.into_json().context("worker response was not valid JSON")?;
        Ok(body.outcomes)
    }
}
