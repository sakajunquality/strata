//! Calls out to the application that Strata serves.
//!
//! Strata stores and serves repositories; who may use them, and under which
//! rules, is the application's business. It answers over HTTP: Strata POSTs
//! a JSON question and acts on the JSON answer. No answer means no.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// The application is asked while a git client waits.
const TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct Hooks {
    client: reqwest::Client,
    /// Sent as a Bearer token, so that the application knows who is asking.
    secret: Option<String>,
}

impl Hooks {
    pub(crate) fn new(secret: Option<String>) -> Hooks {
        let client = reqwest::Client::builder().timeout(TIMEOUT).build().expect("the HTTP client has static settings");
        Hooks { client, secret }
    }

    pub(crate) async fn ask<Q: Serialize, A: DeserializeOwned>(&self, url: &str, question: &Q) -> anyhow::Result<A> {
        let mut request = self.client.post(url).json(question);
        if let Some(secret) = &self.secret {
            request = request.bearer_auth(secret);
        }
        let response = request.send().await?.error_for_status()?;
        Ok(response.json().await?)
    }
}
