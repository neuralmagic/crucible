//! The run's decision desk: the controller that dispatched the run, reached at
//! `/api/runs/{run}/decisions` with the pod token it handed the run.

use std::time::Duration;

use crate::report::session::SessionEvent;
use crucible::plan::decide::DecisionDesk;
use crucible_contract::decision_request::{OpenRequest, RequestState, RequestStatus};

/// Overrides [`DEFAULT_POLL`].
pub const POLL_ENV: &str = "CRUCIBLE_DECISION_POLL_SECS";
const DEFAULT_POLL: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct HttpDesk<'a> {
    /// `{controller}/api/runs/{run}/decisions`.
    base: String,
    token: String,
    client: reqwest::blocking::Client,
    interval: Duration,
    /// The session log `decision_wait` lines go to.
    log: Option<&'a std::fs::File>,
}

impl<'a> HttpDesk<'a> {
    /// `None` unless the controller handed the run its URL, the run's id and its pod token.
    pub fn from_env(log: Option<&'a std::fs::File>) -> Option<Self> {
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let controller = env(crucible_contract::ENV_CONTROLLER_URL)?;
        let run = env(crucible_contract::ENV_RUN_NAME)?;
        let token = env(crucible_contract::ENV_POD_TOKEN)?;
        let client = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .ok()?;
        let interval = env(POLL_ENV)
            .and_then(|v| v.parse().ok())
            .map_or(DEFAULT_POLL, Duration::from_secs);
        Some(HttpDesk {
            base: format!(
                "{}/api/runs/{}/decisions",
                controller.trim_end_matches('/'),
                crucible_contract::artifact::path_segment(&run)
            ),
            token,
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
        let state = Self::decode(
            self.client
                .post(&self.base)
                .bearer_auth(&self.token)
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
        Self::decode(
            self.client
                .get(format!("{}/{id}", self.base))
                .bearer_auth(&self.token)
                .send(),
        )
    }

    fn interval(&self) -> Duration {
        self.interval
    }
}
