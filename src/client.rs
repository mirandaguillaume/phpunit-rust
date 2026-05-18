use crate::types::{TestOutcome, TestRequest};
use anyhow::{anyhow, Context, Result};
use std::time::Duration;

pub struct WorkerClient {
    url: String,
    agent: ureq::Agent,
}

impl WorkerClient {
    pub fn new(url: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(30))
            .build();
        Self { url: url.into(), agent }
    }

    pub fn run_test(&self, req: &TestRequest) -> Result<TestOutcome> {
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
        let outcome: TestOutcome = resp.into_json().context("worker response was not valid JSON")?;
        Ok(outcome)
    }
}
