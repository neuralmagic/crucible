//! The harness-agnostic agent event: crucible's view of one thing the agent did, and the on-disk
//! shape of the session log both the engine and the controller's SSE relay read. Serde wire types
//! only; the cost/pricing helpers that consume them live in `crucible`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A telemetry sample the agent emits roughly every few thousand tokens.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Tokens {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub rate: Option<f64>,
    /// Authoritative live cost from the OTEL collector (None when telemetry is off).
    #[serde(default)]
    pub cost_usd: Option<f64>,
}

/// Per-model token + cost rollup from the OTEL summary.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct ModelUsage {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub cost_usd: f64,
}

/// One normalized event from the agent run. `#[serde(tag = "kind")]` decodes the
/// NDJSON directly; unknown fields (like `v`) are ignored, and an unknown `kind`
/// fails to decode so the caller can fall back to the scraper.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    /// One-shot run header. The schema also carries backend/harness/workdir;
    /// serde ignores the fields we don't render, so only `model` is decoded.
    Meta {
        #[serde(default)]
        model: String,
    },
    /// A lifecycle banner (section/detail/info) with no more specific kind.
    Log {
        #[serde(default)]
        level: String,
        #[serde(default)]
        label: String,
        #[serde(default)]
        value: Option<String>,
    },
    /// A pre/post gate result (sensitive-files, gitleaks, ...).
    Gate {
        #[serde(default)]
        phase: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        status: String,
        #[serde(default)]
        error: Option<String>,
    },
    /// Agent session init (version, model, tool/agent counts).
    Init {
        #[serde(default)]
        model: String,
        #[serde(default)]
        tools: u32,
        #[serde(default)]
        agents: u32,
    },
    /// Streamed chain-of-thought text.
    Thinking {
        #[serde(default)]
        delta: String,
    },
    /// Streamed assistant text.
    Text {
        #[serde(default)]
        delta: String,
    },
    /// A tool or subagent invocation, with the compact summary line. `input`/`result`
    /// are populated only under verbose tool IO (`CRUCIBLE_SESSION_TOOL_IO=full`),
    /// bounded producer-side; the default stays name+summary so the log stays compact.
    Tool {
        #[serde(default)]
        name: String,
        #[serde(default)]
        summary: String,
        #[serde(default)]
        subagent: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
    },
    /// A token/cost telemetry sample.
    Tokens(Tokens),
    /// An API retry (overload/backoff).
    Retry {
        #[serde(default)]
        attempt: u32,
        #[serde(default)]
        max: u32,
        #[serde(default)]
        error: String,
    },
    /// An agent or stream error.
    Error {
        #[serde(default)]
        error_type: String,
        #[serde(default)]
        message: String,
    },
    /// Terminal run result, carrying the cost and turn count.
    ///
    /// `is_error` is decoupled from `subtype`: the CLI can emit `subtype:"success"`
    /// with `is_error:true` (e.g. a credential-less no-op turn), so the loop reads
    /// `is_error` rather than `subtype` to tell a real turn from a no-op.
    Result {
        #[serde(default)]
        subtype: String,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        turns: u32,
        #[serde(default)]
        cost_usd: f64,
        #[serde(default)]
        error: Option<String>,
    },
    /// Post-run OTEL rollup: authoritative cost, per-model usage, API latency.
    OtelSummary {
        #[serde(default)]
        cost_usd: f64,
        #[serde(default)]
        total: u64,
        #[serde(default)]
        models: Vec<ModelUsage>,
        #[serde(default)]
        api_requests: u32,
        #[serde(default)]
        api_ms: f64,
        #[serde(default)]
        active_seconds: f64,
    },
    /// The agent process exit code.
    Exit {
        #[serde(default)]
        code: i32,
    },
    /// One line from the sandbox's own log, replayed into the turn's stream. The engine decides
    /// whether a line reports a policy denial while it holds the structured line, so it says so
    /// here instead of leaving a consumer to re-derive it from the message text.
    SandboxLog {
        #[serde(default)]
        ts_ms: i64,
        #[serde(default)]
        level: String,
        /// The subsystem that emitted it (`proxy`, `supervisor`, …).
        #[serde(default)]
        target: String,
        #[serde(default)]
        message: String,
        /// The line reports a blocked egress attempt.
        #[serde(default)]
        denial: bool,
        /// The line's own structured fields, verbatim.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        fields: BTreeMap<String, String>,
    },
    /// A line we could not classify, passed through verbatim so nothing is lost.
    Raw { text: String, stream: RawStream },
}

/// Which OS stream an unclassified line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RawStream {
    Stdout,
    Stderr,
}

impl AgentEvent {
    /// Decode one NDJSON line if it is a versioned event object. Returns `None`
    /// for anything that is not `{"v":1,...}` so the scraper can take over.
    pub fn from_json_line(line: &str) -> Option<AgentEvent> {
        let t = line.trim_start();
        if !t.starts_with("{\"v\"") && !t.starts_with("{ \"v\"") {
            // Cheap reject: only attempt serde on lines that look like our schema.
            if !(t.starts_with('{') && t.contains("\"kind\"") && t.contains("\"v\"")) {
                return None;
            }
        }
        crate::json::from_str::<AgentEvent>(t).ok()
    }
}

#[cfg(test)]
mod tests {
    /// A sandbox line reaches a consumer as data: its level, its subsystem, its own fields, and
    /// the engine's verdict on whether it reports a denial — none of which survive a message
    /// string with a prefix on it.
    #[test]
    fn a_sandbox_log_line_carries_its_structure() {
        let ev = AgentEvent::SandboxLog {
            ts_ms: 1_787_759_206_035,
            level: "WARN".to_string(),
            target: "proxy".to_string(),
            message: "CONNECT denied api.example.com:443".to_string(),
            denial: true,
            fields: BTreeMap::from([("denial_stage".to_string(), "connect".to_string())]),
        };
        let json = serde_json::to_value(&ev).expect("serialize");
        assert_eq!(json["kind"], "sandbox_log");
        assert_eq!(json["denial"], true);
        assert_eq!(json["target"], "proxy");
        assert_eq!(json["fields"]["denial_stage"], "connect");

        let back: AgentEvent = crate::json::from_str(&json.to_string()).expect("deserialize");
        let AgentEvent::SandboxLog {
            denial,
            target,
            fields,
            ts_ms,
            ..
        } = back
        else {
            panic!("decodes as itself");
        };
        assert!(denial);
        assert_eq!(target, "proxy");
        assert_eq!(ts_ms, 1_787_759_206_035);
        assert_eq!(fields["denial_stage"], "connect");
    }

    /// An ordinary line carries no fields, and says so by omission rather than by an empty map.
    #[test]
    fn a_plain_sandbox_line_omits_empty_fields() {
        let ev = AgentEvent::SandboxLog {
            ts_ms: 0,
            level: "INFO".to_string(),
            target: "supervisor".to_string(),
            message: "starting".to_string(),
            denial: false,
            fields: BTreeMap::new(),
        };
        let json = serde_json::to_value(&ev).expect("serialize");
        assert!(json.get("fields").is_none(), "{json}");
        assert_eq!(json["denial"], false);
    }

    use super::*;

    type Case = (&'static str, fn(&AgentEvent) -> bool);

    /// One line per kind the producer emits. This is the cross-repo contract: every
    /// producer kind must decode here.
    #[test]
    fn decodes_every_schema_kind() {
        let cases: &[Case] = &[
            (
                r#"{"v":1,"kind":"meta","backend":"local","harness":"claude-code","model":"claude-opus-4-6","workdir":"/w"}"#,
                |e| matches!(e, AgentEvent::Meta { model } if model == "claude-opus-4-6"),
            ),
            (
                r#"{"v":1,"kind":"log","level":"section","label":"Running checks","value":null}"#,
                |e| matches!(e, AgentEvent::Log { level, .. } if level == "section"),
            ),
            (
                r#"{"v":1,"kind":"gate","phase":"post","name":"gitleaks","status":"passed","error":null}"#,
                |e| matches!(e, AgentEvent::Gate { status, .. } if status == "passed"),
            ),
            (
                r#"{"v":1,"kind":"init","agent_version":"1.2.3","model":"m","permission_mode":"bypassPermissions","tools":3,"mcp_servers":[],"agents":2,"plugins":["example-plugin"]}"#,
                |e| matches!(e, AgentEvent::Init { tools, agents, .. } if *tools == 3 && *agents == 2),
            ),
            (
                r#"{"v":1,"kind":"thinking","delta":"x"}"#,
                |e| matches!(e, AgentEvent::Thinking { delta } if delta == "x"),
            ),
            (
                r#"{"v":1,"kind":"text","delta":"y"}"#,
                |e| matches!(e, AgentEvent::Text { delta } if delta == "y"),
            ),
            (
                r#"{"v":1,"kind":"tool","name":"Edit","summary":"p.go: x","subagent":false,"input":{"file_path":"p.go"}}"#,
                |e| {
                    matches!(e, AgentEvent::Tool { name, subagent, input, .. }
                    if name == "Edit" && !*subagent
                    && input.as_ref().and_then(|i| i.get("file_path")).and_then(|p| p.as_str()) == Some("p.go"))
                },
            ),
            (
                // A verbose-mode result excerpt round-trips; compact lines above stay valid.
                r#"{"v":1,"kind":"tool","name":"Bash","summary":"result","subagent":false,"result":"ok\n"}"#,
                |e| {
                    matches!(e, AgentEvent::Tool { name, result, input, .. }
                    if name == "Bash" && result.as_deref() == Some("ok\n") && input.is_none())
                },
            ),
            (
                r#"{"v":1,"kind":"tokens","input":0,"output":6000,"cache_read":0,"cache_write":0,"total":6000,"rate":null}"#,
                |e| matches!(e, AgentEvent::Tokens(t) if t.total == 6000 && t.rate.is_none()),
            ),
            (
                r#"{"v":1,"kind":"retry","attempt":1,"max":5,"delay_ms":2000,"error":"overloaded_error"}"#,
                |e| matches!(e, AgentEvent::Retry { attempt, max, .. } if *attempt == 1 && *max == 5),
            ),
            (
                r#"{"v":1,"kind":"error","error_type":"overloaded","message":"busy"}"#,
                |e| matches!(e, AgentEvent::Error { error_type, .. } if error_type == "overloaded"),
            ),
            (
                r#"{"v":1,"kind":"result","subtype":"success","stop_reason":"end_turn","duration_ms":1,"api_duration_ms":1,"ttft_ms":1,"turns":12,"cost_usd":0.84}"#,
                |e| matches!(e, AgentEvent::Result { turns, cost_usd, is_error, .. } if *turns == 12 && (*cost_usd - 0.84).abs() < 1e-9 && !*is_error),
            ),
            (
                // subtype "success" but the CLI flagged the turn as an error;
                // is_error and the text must survive decode.
                r#"{"v":1,"kind":"result","subtype":"success","stop_reason":"end_turn","turns":1,"cost_usd":0.0,"is_error":true,"error":"Not logged in"}"#,
                |e| matches!(e, AgentEvent::Result { is_error, error, .. } if *is_error && error.as_deref() == Some("Not logged in")),
            ),
            (
                r#"{"v":1,"kind":"tokens","input":1,"output":2,"total":3,"rate":null,"cost_usd":0.5}"#,
                |e| matches!(e, AgentEvent::Tokens(t) if t.cost_usd == Some(0.5)),
            ),
            (
                r#"{"v":1,"kind":"otel_summary","cost_usd":0.84,"total":89200,"models":[{"model":"claude-opus-4-6","input":1200,"output":340,"cache_read":88000,"cache_write":0,"cost_usd":0.84}],"api_requests":12,"api_ms":80110.0,"active_seconds":372.0}"#,
                |e| matches!(e, AgentEvent::OtelSummary { api_requests, models, .. } if *api_requests == 12 && models.len() == 1),
            ),
            (
                r#"{"v":1,"kind":"exit","code":0}"#,
                |e| matches!(e, AgentEvent::Exit { code } if *code == 0),
            ),
        ];
        for (line, check) in cases {
            let ev = AgentEvent::from_json_line(line)
                .unwrap_or_else(|| panic!("failed to decode: {line}"));
            assert!(check(&ev), "wrong variant for: {line}");
        }
    }

    #[test]
    fn unknown_kind_falls_through_to_none() {
        assert!(AgentEvent::from_json_line(r#"{"v":1,"kind":"future_thing","x":1}"#).is_none());
    }
}
