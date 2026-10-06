//! The run's decision desk: the controller's ingest surface, reached with the pod's ingest
//! credential (a local run's controller hands it the same three variables).

use std::time::Duration;

use crate::report::ingest_client::IngestConfig;
use crate::report::session::SessionEvent;
use crucible::plan::decide::DecisionDesk;
use crucible_contract::decision_request::{OpenRequest, RequestState, RequestStatus};

/// Overrides [`DEFAULT_POLL`].
pub const POLL_ENV: &str = "CRUCIBLE_DECISION_POLL_SECS";
const DEFAULT_POLL: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct HttpDesk<'a> {
    cfg: IngestConfig,
    client: reqwest::blocking::Client,
    interval: Duration,
    /// The session log `decision_wait` lines go to.
    log: Option<&'a std::fs::File>,
}

impl<'a> HttpDesk<'a> {
    /// `None` when the run has no ingest surface to open requests on.
    pub fn from_env(log: Option<&'a std::fs::File>) -> Option<Self> {
        let cfg = IngestConfig::from_env()?;
        let client = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .ok()?;
        let interval = std::env::var(POLL_ENV)
            .ok()
            .and_then(|v| v.parse().ok())
            .map_or(DEFAULT_POLL, Duration::from_secs);
        Some(HttpDesk {
            cfg,
            client,
            interval,
            log,
        })
    }

    fn decode(resp: reqwest::Result<reqwest::blocking::Response>) -> Result<RequestState, String> {
        let resp = resp.map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(format!("HTTP {status}: {}", body.trim()));
        }
        resp.json()
            .map_err(|e| format!("decoding the request state: {e}"))
    }
}

impl DecisionDesk for HttpDesk<'_> {
    fn open(&self, request: &OpenRequest) -> Result<RequestState, String> {
        let token = self
            .cfg
            .bearer()
            .map_err(|e| format!("reading the ingest token: {e}"))?;
        let state = Self::decode(
            self.client
                .post(self.cfg.pod_url("decisions"))
                .bearer_auth(token)
                .json(request)
                .send(),
        )?;
        if let (Some(log), RequestStatus::Open) = (self.log, &state.status) {
            use std::io::Write;
            let mut w = log;
            let _ = writeln!(
                w,
                "{}",
                crate::report::session::encode(&SessionEvent::DecisionWait {
                    task: request.task.clone(),
                    request: state.id.clone(),
                    evidence_digest: state.evidence_digest.clone(),
                    expires_at: state.expires_at.clone(),
                })
            );
        }
        Ok(state)
    }

    fn poll(&self, id: &str) -> Result<RequestState, String> {
        let token = self
            .cfg
            .bearer()
            .map_err(|e| format!("reading the ingest token: {e}"))?;
        Self::decode(
            self.client
                .get(self.cfg.pod_url(&format!("decisions/{id}")))
                .bearer_auth(token)
                .send(),
        )
    }

    fn interval(&self) -> Duration {
        self.interval
    }
}
