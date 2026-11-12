//! Calls out to the application that Strata serves.
//!
//! Strata stores and serves repositories; who may use them, and under which
//! rules, is the application's business. It answers over HTTP: Strata POSTs
//! a JSON question and acts on the JSON answer. No answer means no.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::Metrics;

/// The application is asked while a git client waits.
const TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct Hooks {
    client: reqwest::Client,
    /// Sent as a Bearer token, so that the application knows who is asking.
    secret: Option<String>,
    metrics: Arc<Metrics>,
}

impl Hooks {
    pub(crate) fn new(secret: Option<String>, metrics: Arc<Metrics>) -> Hooks {
        let client = reqwest::Client::builder().timeout(TIMEOUT).build().expect("the HTTP client has static settings");
        Hooks { client, secret, metrics }
    }

    /// Puts a question to the hook called `hook` in the metrics.
    pub(crate) async fn ask<Q: Serialize, A: DeserializeOwned>(&self, hook: &str, url: &str, question: &Q) -> anyhow::Result<A> {
        let started = Instant::now();
        let answer = self.post(url, question).await;
        self.metrics.hook(hook, answer.is_ok(), started.elapsed());
        answer
    }

    async fn post<Q: Serialize, A: DeserializeOwned>(&self, url: &str, question: &Q) -> anyhow::Result<A> {
        let mut request = self.client.post(url).json(question);
        if let Some(secret) = &self.secret {
            request = request.bearer_auth(secret);
        }
        let response = request.send().await?.error_for_status()?;
        Ok(response.json().await?)
    }
}
