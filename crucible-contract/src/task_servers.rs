//! The MCP servers a run's command and evaluate tasks reach with task tokens. The engine exchanges
//! the run's pod token at the controller's token endpoint for a token per task and server, keeps it
//! in a file, and names the servers to the task in [`ENV_TASK_SERVERS`].

use crate::agent_servers::ServerName;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Task env carrying the task's servers, a JSON list of [`TaskServer`].
pub const ENV_TASK_SERVERS: &str = "CRUCIBLE_TASK_SERVERS";

/// Loop-pod env carrying the servers the run's tasks may reach, a JSON list of [`RunServer`].
pub const ENV_RUN_SERVERS: &str = "CRUCIBLE_RUN_SERVERS";

/// Loop-pod env naming the controller's token endpoint for task tokens.
pub const ENV_TASK_TOKEN_ENDPOINT: &str = "CRUCIBLE_TASK_TOKEN_ENDPOINT";

/// Loop-pod env naming the SPIFFE Workload API socket the engine fetches the pod's JWT-SVID from,
/// set where the cluster gives loop pods one.
pub const ENV_LOOP_SPIFFE_SOCKET: &str = "CRUCIBLE_LOOP_SPIFFE_SOCKET";

/// Loop-pod env naming the audience the pod's JWT-SVID is requested for: the controller's issuer.
pub const ENV_TASK_SVID_AUDIENCE: &str = "CRUCIBLE_TASK_SVID_AUDIENCE";

/// RFC 8693 §3, the `actor_token_type` of a JWT-SVID.
pub const JWT_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:jwt";

/// What follows the trust domain in a loop pod's SPIFFE ID, before the pod's name.
pub const RUN_SPIFFE_PATH: &str = "crucible/run/";

/// RFC 8693 §2.1.
pub const TOKEN_EXCHANGE_GRANT: &str = "urn:ietf:params:oauth:grant-type:token-exchange";

/// The `subject_token_type` of a run's pod token.
pub const POD_TOKEN_TYPE: &str = "urn:crucible:params:oauth:token-type:pod-token";

/// RFC 8693 §3, the `issued_token_type` of an access token.
pub const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";

/// The longest task name a token request may carry.
pub const MAX_TASK_NAME_BYTES: usize = 256;

/// One server a task reaches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskServer {
    pub name: ServerName,
    /// The MCP URL, the audience of the token.
    pub url: String,
    /// The file holding the current access token. The engine rewrites it before the token
    /// expires, so a client reads it for every request.
    pub token_file: PathBuf,
}

/// One server the controller lets a run's tasks reach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunServer {
    pub name: ServerName,
    /// The MCP URL, the audience of its tokens.
    pub url: String,
    /// The scopes its tokens are requested with.
    pub scopes: Vec<String>,
}

pub fn encode_run_servers(servers: &[RunServer]) -> Result<String, serde_json::Error> {
    serde_json::to_string(servers)
}

/// The servers [`ENV_RUN_SERVERS`] carries; an empty value is none.
pub fn decode_run_servers(raw: &str) -> Result<Vec<RunServer>, serde_json::Error> {
    match raw.trim() {
        "" => Ok(Vec::new()),
        raw => serde_json::from_str(raw),
    }
}

pub fn encode(servers: &[TaskServer]) -> Result<String, serde_json::Error> {
    serde_json::to_string(servers)
}

/// The servers [`ENV_TASK_SERVERS`] carries; an empty value is none.
pub fn decode(raw: &str) -> Result<Vec<TaskServer>, serde_json::Error> {
    match raw.trim() {
        "" => Ok(Vec::new()),
        raw => serde_json::from_str(raw),
    }
}

/// Whether `task` may name a task in a token request: non-empty, at most
/// [`MAX_TASK_NAME_BYTES`], and free of whitespace and control characters.
pub fn is_task_name(task: &str) -> bool {
    (1..=MAX_TASK_NAME_BYTES).contains(&task.len())
        && !task.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// The `act.sub` of a task token: the run's Kubernetes user, then the task.
pub fn task_actor(run_pod: &str, task: &str) -> String {
    format!("crucible:run:{run_pod}:task:{task}")
}

#[cfg(test)]
mod tests {
    use crate::task_servers::*;

    #[test]
    fn servers_round_trip_and_an_empty_value_is_none() {
        let servers = vec![TaskServer {
            name: ServerName::parse("kueue").unwrap(),
            url: "http://kueue.example.svc:8850/mcp".into(),
            token_file: "/run/task-tokens/build/kueue".into(),
        }];
        assert_eq!(decode(&encode(&servers).unwrap()).unwrap(), servers);
        assert!(decode("  ").unwrap().is_empty());
        assert!(decode(r#"[{"name":"kueue","url":"u","token_file":"f","token":"t"}]"#).is_err());
    }

    #[test]
    fn run_servers_round_trip_and_an_empty_value_is_none() {
        let servers = vec![RunServer {
            name: ServerName::parse("kueue").unwrap(),
            url: "http://kueue.example.svc:8850/mcp".into(),
            scopes: vec!["kueue.read".into()],
        }];
        assert_eq!(
            decode_run_servers(&encode_run_servers(&servers).unwrap()).unwrap(),
            servers
        );
        assert!(decode_run_servers("").unwrap().is_empty());
        assert!(decode_run_servers(r#"[{"name":"kueue","url":"u"}]"#).is_err());
    }

    #[test]
    fn a_task_name_is_bounded_and_has_no_whitespace() {
        assert!(is_task_name("probe"));
        assert!(is_task_name("probe[cuda-12.4]"));
        assert!(!is_task_name(""));
        assert!(!is_task_name("two words"));
        assert!(!is_task_name("line\nbreak"));
        assert!(!is_task_name(&"a".repeat(MAX_TASK_NAME_BYTES + 1)));
    }

    #[test]
    fn the_actor_names_the_run_and_the_task() {
        assert_eq!(
            task_actor("crucible-run-brave-otter", "probe"),
            "crucible:run:crucible-run-brave-otter:task:probe"
        );
    }
}
