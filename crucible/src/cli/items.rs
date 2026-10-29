//! `crucible items`: the item store of the playbook this run was launched from, for a command
//! task. The controller's URL, the run's id, and its pod token come from the run pod's environment.

use anyhow::{Context, Result};
use crucible_contract::items::{
    ClaimRequest, Conflict, ItemKey, ItemKeys, LabelEdit, LabelName, LabelValue, Op, QueryRequest,
    Refused, ReleaseRequest, Selector, SetRequest, StoreName,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// The exit status of a claim or a set the controller refused.
pub(crate) const REFUSED_EXIT: u8 = 4;

const MAX_ATTEMPTS: u32 = 3;
const BACKOFF: Duration = Duration::from_secs(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(clap::Subcommand)]
pub(crate) enum ItemsAction {
    /// Print the items among the keys that a run has written, as JSON.
    Query {
        #[command(flatten)]
        target: Target,
        /// Only items whose labels match, as in `triaged,verdict!=error`.
        #[arg(long)]
        selector: Option<Selector>,
    },
    /// Hold every key for this run, or none. Exits 4 and prints the conflicts when another live
    /// run holds a key or a key's labels match `--unless`.
    Claim {
        #[command(flatten)]
        target: Target,
        /// Refuse keys whose labels match this selector.
        #[arg(long)]
        unless: Option<Selector>,
        /// Ignore `--unless`. Another live run's hold still refuses.
        #[arg(long)]
        force: bool,
    },
    /// Set (`name=value`) or remove (`name-`) labels on keys this run holds. Exits 4 and prints
    /// the keys it does not hold, changing none.
    Set {
        #[command(flatten)]
        target: Target,
        #[arg(required = true, value_name = "LABEL")]
        labels: Vec<String>,
    },
    /// Drop this run's holds on the keys and print the items released.
    Release {
        #[command(flatten)]
        target: Target,
    },
}

#[derive(clap::Args)]
pub(crate) struct Target {
    /// The playbook's store.
    #[arg(long)]
    store: StoreName,
    /// An item key. Repeatable.
    #[arg(long = "key", value_name = "KEY")]
    keys: Vec<ItemKey>,
    /// A file of item keys, one per line; `-` reads stdin.
    #[arg(long, value_name = "PATH")]
    keys_from: Option<PathBuf>,
}

impl Target {
    fn keys(&self) -> Result<ItemKeys> {
        let mut keys = self.keys.clone();
        if let Some(path) = &self.keys_from {
            let raw = match path.as_os_str() == "-" {
                true => {
                    std::io::read_to_string(std::io::stdin()).context("reading keys from stdin")?
                }
                false => std::fs::read_to_string(path)
                    .with_context(|| format!("reading keys from {}", path.display()))?,
            };
            for line in raw.lines().map(str::trim).filter(|l| !l.is_empty()) {
                keys.push(line.parse()?);
            }
        }
        Ok(ItemKeys::new(keys)?)
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ItemsCliError {
    #[error("`crucible items` runs inside a playbook run pod: {0} is unset")]
    NoEnv(&'static str),
    #[error("label {0:?} is neither `name=value` nor `name-`")]
    LabelArg(String),
    #[error("the item store refused the {op}: HTTP {status}: {body}")]
    Rejected {
        op: &'static str,
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("the item {op} failed after {MAX_ATTEMPTS} attempts: {last}")]
    Unreachable { op: &'static str, last: String },
    /// A claim or a set the controller refused, after its conflicts were printed.
    #[error("the item store refused the {op} over {keys}")]
    Refused { op: &'static str, keys: String },
}

fn label_edit(args: &[String]) -> Result<LabelEdit> {
    let mut edit = BTreeMap::new();
    for arg in args {
        let (name, value) = match (arg.split_once('='), arg.strip_suffix('-')) {
            (Some((name, value)), _) => (name, Some(LabelValue::new(value)?)),
            (None, Some(name)) => (name, None),
            (None, None) => return Err(ItemsCliError::LabelArg(arg.clone()).into()),
        };
        edit.insert(LabelName::new(name)?, value);
    }
    Ok(LabelEdit::new(edit)?)
}

/// Where this run's item requests go, from the run pod's environment.
struct Endpoint {
    base: String,
    run: String,
    token: String,
}

impl Endpoint {
    fn from_env() -> Result<Self> {
        let var = |name: &'static str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or(ItemsCliError::NoEnv(name))
        };
        Ok(Self {
            base: var(crucible_contract::ENV_CONTROLLER_URL)?
                .trim_end_matches('/')
                .to_string(),
            run: var(crucible_contract::ENV_RUN_NAME)?,
            token: var(crucible_contract::ENV_POD_TOKEN)?,
        })
    }

    /// POST `body` for `op`, retrying a transport failure or a 5xx; every op repeats safely for
    /// one run. `Ok(Err(_))` is a 409 refusal.
    fn post(
        &self,
        store: &StoreName,
        op: Op,
        body: &impl serde::Serialize,
    ) -> Result<Result<String, Refused>> {
        let client = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building the item store client")?;
        let url = format!(
            "{}{}",
            self.base,
            crucible_contract::items::path(&self.run, store, op)
        );
        let mut last = String::new();
        for attempt in 1..=MAX_ATTEMPTS {
            match client.post(&url).bearer_auth(&self.token).json(body).send() {
                Ok(r) if r.status().is_success() => {
                    return r.text().context("reading the item store reply").map(Ok);
                }
                Ok(r) if r.status() == reqwest::StatusCode::CONFLICT => {
                    let text = r.text().context("reading the item store refusal")?;
                    let refused: Refused = serde_json::from_str(&text)
                        .with_context(|| format!("parsing the item store refusal {text:?}"))?;
                    return Ok(Err(refused));
                }
                Ok(r) if r.status().is_server_error() => {
                    last = format!(
                        "HTTP {}: {}",
                        r.status(),
                        r.text().unwrap_or_default().trim()
                    );
                }
                Ok(r) => {
                    let status = r.status();
                    return Err(ItemsCliError::Rejected {
                        op: op.as_str(),
                        status,
                        body: r.text().unwrap_or_default().trim().to_string(),
                    }
                    .into());
                }
                Err(e) => last = e.without_url().to_string(),
            }
            if attempt < MAX_ATTEMPTS {
                std::thread::sleep(BACKOFF * attempt);
            }
        }
        Err(ItemsCliError::Unreachable {
            op: op.as_str(),
            last,
        }
        .into())
    }
}

pub(crate) fn run(action: &ItemsAction) -> Result<()> {
    let endpoint = Endpoint::from_env()?;
    let (target, op, body) = match action {
        ItemsAction::Query { target, selector } => (
            target,
            Op::Query,
            serde_json::to_value(QueryRequest {
                keys: target.keys()?,
                selector: selector.clone(),
            }),
        ),
        ItemsAction::Claim {
            target,
            unless,
            force,
        } => (
            target,
            Op::Claim,
            serde_json::to_value(ClaimRequest {
                keys: target.keys()?,
                unless: unless.clone(),
                force: *force,
            }),
        ),
        ItemsAction::Set { target, labels } => (
            target,
            Op::Set,
            serde_json::to_value(SetRequest {
                keys: target.keys()?,
                labels: label_edit(labels)?,
            }),
        ),
        ItemsAction::Release { target } => (
            target,
            Op::Release,
            serde_json::to_value(ReleaseRequest {
                keys: target.keys()?,
            }),
        ),
    };
    let body = body.context("encoding the item request")?;
    match endpoint.post(&target.store, op, &body)? {
        Ok(items) => {
            println!("{}", items.trim());
            Ok(())
        }
        Err(refused) => {
            println!(
                "{}",
                serde_json::to_string(&refused).context("printing the refusal")?
            );
            let keys: Vec<String> = refused
                .conflicts
                .iter()
                .map(|c| match c {
                    Conflict::Held { key, .. }
                    | Conflict::Matched { key, .. }
                    | Conflict::NotHeld { key } => key.to_string(),
                })
                .collect();
            Err(ItemsCliError::Refused {
                op: op.as_str(),
                keys: keys.join(", "),
            }
            .into())
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::items::*;

    #[test]
    fn labels_set_with_equals_and_remove_with_a_trailing_dash() {
        let edit = label_edit(&[
            "verdict=not_affected".into(),
            "triaged=".into(),
            "pending-".into(),
        ])
        .unwrap();
        assert_eq!(
            serde_json::to_value(&edit).unwrap(),
            serde_json::json!({"pending": null, "triaged": "", "verdict": "not_affected"})
        );
        for bad in ["verdict", "a b=c", "v=a b", "-"] {
            assert!(label_edit(&[bad.into()]).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn keys_come_from_flags_and_a_file_and_are_checked_together() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("keys");
        std::fs::write(&file, "K-2\n\n  K-3  \n").unwrap();
        let target = Target {
            store: "tickets".parse().unwrap(),
            keys: vec!["K-1".parse().unwrap()],
            keys_from: Some(file.clone()),
        };
        assert_eq!(target.keys().unwrap().sorted(), ["K-1", "K-2", "K-3"]);
        std::fs::write(&file, "K-1\n").unwrap();
        assert!(target.keys().is_err(), "a key named twice");
        let empty = Target {
            store: "tickets".parse().unwrap(),
            keys: vec![],
            keys_from: None,
        };
        assert!(empty.keys().is_err());
    }

    /// A stand-in for the controller's item routes on loopback: answers each request with the
    /// next of `replies` and hands back what it received.
    fn controller(
        replies: Vec<(&'static str, &'static str)>,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for (status, body) in replies {
                let (mut conn, _) = listener.accept().unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let request = loop {
                    let n = conn.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if rest.len() >= len || n == 0 {
                            break text;
                        }
                    }
                };
                tx.send(request).unwrap();
                let reply = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                conn.write_all(reply.as_bytes()).unwrap();
            }
        });
        (format!("http://{addr}"), rx)
    }

    fn endpoint(base: String) -> Endpoint {
        Endpoint {
            base,
            run: "playbook_triage_01/a".into(),
            token: "crr_id_secret".into(),
        }
    }

    fn claim_body() -> ClaimRequest {
        ClaimRequest {
            keys: ItemKeys::new(vec!["K-1".parse().unwrap()]).unwrap(),
            unless: Some("triaged".parse().unwrap()),
            force: false,
        }
    }

    #[test]
    fn a_claim_posts_to_the_runs_store_with_its_pod_token() {
        let (base, seen) = controller(vec![("200 OK", r#"{"items":[]}"#)]);
        let store: StoreName = "tickets".parse().unwrap();
        let reply = endpoint(base)
            .post(&store, Op::Claim, &claim_body())
            .unwrap();
        assert_eq!(reply.unwrap(), r#"{"items":[]}"#);
        let request = seen.recv().unwrap();
        assert!(
            request
                .starts_with("POST /api/items/runs/playbook_triage_01%2Fa/tickets/claim HTTP/1.1"),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer crr_id_secret\r\n"),
            "{request}"
        );
        assert!(
            request.ends_with(r#"{"keys":["K-1"],"unless":"triaged","force":false}"#),
            "{request}"
        );
    }

    #[test]
    fn a_conflict_is_a_refusal_and_a_server_error_is_retried() {
        let (base, seen) = controller(vec![
            ("503 Service Unavailable", r#"{"error":"busy"}"#),
            (
                "409 Conflict",
                r#"{"conflicts":[{"reason":"held","key":"K-1","run":"run-b"}]}"#,
            ),
        ]);
        let store: StoreName = "tickets".parse().unwrap();
        let refused = endpoint(base)
            .post(&store, Op::Claim, &claim_body())
            .unwrap()
            .unwrap_err();
        assert_eq!(
            refused.conflicts,
            [crucible_contract::items::Conflict::Held {
                key: "K-1".parse().unwrap(),
                run: "run-b".into(),
            }]
        );
        assert_eq!(seen.try_iter().count(), 2);
    }

    #[test]
    fn a_client_error_fails_at_once_with_the_controllers_message() {
        let (base, seen) = controller(vec![("403 Forbidden", r#"{"error":"not yours"}"#)]);
        let store: StoreName = "tickets".parse().unwrap();
        let err = endpoint(base)
            .post(&store, Op::Claim, &claim_body())
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 403"), "{err:#}");
        assert!(err.to_string().contains("not yours"), "{err:#}");
        assert!(!err.to_string().contains("crr_id_secret"), "{err:#}");
        assert_eq!(seen.try_iter().count(), 1);
    }
}
