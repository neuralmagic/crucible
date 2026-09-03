//! The `distress` tool's broker-side half: the operator page (a Slack webhook) plus the handoff
//! files the loop engine reads.
//!
//! Two channels, chosen by severity:
//!
//! ```text
//!   error       -> <storage>/distress          (the park marker; engine suspends the run)
//!   info | warn -> <storage>/distress-notes.jsonl (appended; folded into the next decided row)
//! ```
//!
//! The marker is the pending-approval token: the engine parks while it exists and the operator's
//! `rm` is the grant. Slack delivery is fire-and-forget on purpose (the park must still happen
//! with Slack down), so a failed POST is a log line, never a tool error. The marker write is the
//! opposite: it IS the suspend, so a failed write fails the tool.

// rmcp's OWN schemars (1.x), the version its tool-arg schemas are built against; the crate also
// depends on schemars 0.8, and deriving against that one fails the tool-router bound.
use rmcp::schemars;
use serde::Deserialize;
use std::path::PathBuf;

/// How loud the signal is. A closed set on the wire (the agent picks one of three) and the only
/// thing that decides whether the run suspends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warn,
    Error,
}

impl Severity {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
    }

    /// Slack attachment color, matching Slack's own status palette.
    fn color(self) -> &'static str {
        match self {
            Severity::Info => "#439FE0",
            Severity::Warn => "#ECB22E",
            Severity::Error => "#E01E5A",
        }
    }

    fn emoji(self) -> &'static str {
        match self {
            Severity::Info => "ℹ️",
            Severity::Warn => "⚠️",
            Severity::Error => "🚨",
        }
    }
}

/// What a distress call does, given its severity and whether the run is already suspended.
/// Pure state machine, split from the IO so every combination is unit-tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// First `error`: page the operator and write the park marker.
    PostAndSuspend,
    /// `info`/`warn`: page the operator and leave a note; the run continues.
    PostAndNote,
    /// `error` while the marker exists: idempotent no-op (no second page, no rewrite).
    AlreadySuspended,
}

fn decide(severity: Severity, already_suspended: bool) -> Action {
    match severity {
        Severity::Error if already_suspended => Action::AlreadySuspended,
        Severity::Error => Action::PostAndSuspend,
        Severity::Info | Severity::Warn => Action::PostAndNote,
    }
}

/// The park marker the engine polls. Its presence suspends the run; deleting it resumes.
fn marker_path() -> PathBuf {
    forge::storage_root().join("distress")
}

/// Append-only info/warn notes, consumed by the engine at the next decided row.
fn notes_path() -> PathBuf {
    forge::storage_root().join("distress-notes.jsonl")
}

/// The engine's per-iteration stamp, best-effort: absent on an older driver or before the first
/// iteration, in which case the page prints `?` rather than lying about an iteration.
fn turn_meta_path() -> PathBuf {
    forge::storage_root().join("turn-meta.json")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Write the park marker atomically (tmp + rename): the engine polls this path every 250ms and
/// must never read a half-written marker as malformed.
fn write_marker(reason: &str, evidence: &[String]) -> std::io::Result<()> {
    let path = marker_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::json!({
        "reason": reason,
        "evidence": evidence,
        "ts_ms": now_ms(),
        "turn_token": crate::turn::current_token(),
    })
    .to_string();
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)
}

fn append_note(severity: Severity, reason: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let path = notes_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::json!({
        "severity": severity.as_str(),
        "reason": reason,
        "ts_ms": now_ms(),
    })
    .to_string();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{line}")
}

/// The iteration the engine last stamped, as display text.
fn iteration() -> String {
    #[derive(Deserialize)]
    struct TurnMeta {
        iter: u32,
    }
    std::fs::read_to_string(turn_meta_path())
        .ok()
        .and_then(|s| serde_json::from_str::<TurnMeta>(&s).ok())
        .map(|m| m.iter.to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// The W3C trace id of the current turn, for the Datadog deep link. `None` when the engine hasn't
/// written a traceparent (a local run, an older driver) or the value is malformed.
fn trace_id(traceparent: &str) -> Option<String> {
    let parts: Vec<&str> = traceparent.trim().split('-').collect();
    let [_version, trace, _span, _flags] = parts[..] else {
        return None;
    };
    let ok = trace.len() == 32
        && trace.chars().all(|c| c.is_ascii_hexdigit())
        && trace.chars().any(|c| c != '0');
    ok.then(|| trace.to_string())
}

/// Everything the Slack page prints, resolved from the environment + handoff files so [`payload`]
/// stays pure.
pub(crate) struct Page {
    pub severity: Severity,
    pub run: String,
    pub pod: String,
    pub namespace: String,
    pub iteration: String,
    pub reason: String,
    pub evidence: Vec<String>,
    /// Datadog trace link, absent when this turn has no traceparent.
    pub trace_url: Option<String>,
}

impl Page {
    fn from_env(severity: Severity, reason: &str, evidence: &[String]) -> Self {
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        let pod = env("CRUCIBLE_POD_NAME");
        let run = match env("CRUCIBLE_RUN_NAME") {
            s if s.is_empty() => pod.clone(),
            s => s,
        };
        let dd_base = std::env::var("DATADOG_BASE_URL")
            .unwrap_or_else(|_| "https://app.datadoghq.com".to_string());
        Self {
            severity,
            run,
            pod,
            namespace: env("CRUCIBLE_POD_NAMESPACE"),
            iteration: iteration(),
            reason: reason.to_string(),
            evidence: evidence.to_vec(),
            trace_url: trace_id(&crate::turn::current_traceparent())
                .map(|t| format!("{}/apm/trace/{t}", dd_base.trim_end_matches('/'))),
        }
    }
}

/// Cap the evidence so a pathological agent can't post a novel into the channel.
const MAX_EVIDENCE_ITEMS: usize = 5;
const MAX_EVIDENCE_CHARS: usize = 300;

/// The Slack webhook body. Pure so the shape is testable without a socket.
pub(crate) fn payload(page: &Page) -> serde_json::Value {
    let mut text = format!("*Reason:* {}", page.reason);
    if !page.evidence.is_empty() {
        text.push_str("\n*Evidence:*");
        for item in page.evidence.iter().take(MAX_EVIDENCE_ITEMS) {
            let capped: String = item.chars().take(MAX_EVIDENCE_CHARS).collect();
            text.push_str(&format!("\n• {capped}"));
        }
    }
    if let Some(url) = &page.trace_url {
        text.push_str(&format!("\n*Trace:* {url}"));
    }
    // Copy-paste operator one-liners: clearing the marker IS the resume grant.
    text.push_str(&format!(
        "\n```\noc exec {pod} -n {ns} -- rm /var/lib/forge/distress\noc logs {pod} -n {ns} --tail=100\n```",
        pod = page.pod,
        ns = page.namespace,
    ));
    let field = |title: &str, value: &str| serde_json::json!({ "title": title, "value": value, "short": true });
    serde_json::json!({
        "attachments": [{
            "color": page.severity.color(),
            "title": format!("{} crucible distress: {}", page.severity.emoji(), page.run),
            "text": text,
            "fields": [
                field("run", &page.run),
                field("pod", &page.pod),
                field("namespace", &page.namespace),
                field("iteration", &page.iteration),
            ],
            "mrkdwn_in": ["text"],
        }]
    })
}

/// POST the page. Blocking (the caller is already on a blocking thread) and fire-and-forget: a
/// dead webhook must not fail the tool, because the suspend itself is the load-bearing half.
pub(crate) fn post(webhook_url: &str, payload: &serde_json::Value) {
    if let Err(error) = crate::slack::post(webhook_url, payload) {
        tracing::warn!(%error, "posting the distress page to slack failed");
    }
}

/// The tool body: decide, write the handoff file, page, reply. `Ok` is a status-tagged JSON line
/// the agent parses; `Err` is a message the caller renders as a tool error.
pub(crate) fn distress(
    severity: Severity,
    reason: &str,
    evidence: &[String],
) -> Result<String, String> {
    let action = decide(severity, marker_path().exists());
    if action == Action::AlreadySuspended {
        return Ok(r#"{"status":"suspended"}"#.to_string());
    }
    // File first: with Slack unreachable the park must still happen.
    match action {
        // The marker IS the suspend. If it didn't land, the engine will never park, so replying
        // "suspending" would send the agent off to wrap up while the run keeps burning budget.
        // Fail the tool instead (a retry can succeed) and post nothing: a page whose `rm` one-liner
        // names a file that doesn't exist is worse than no page.
        Action::PostAndSuspend => {
            if let Err(e) = write_marker(reason, evidence) {
                tracing::warn!(error = %e, "writing the distress park marker failed");
                return Err(format!(
                    "the distress park marker could not be written ({e}); the run is NOT suspended"
                ));
            }
        }
        // A note is cosmetic next to the page that already went out, so a failed append is a log
        // line, not a tool error.
        Action::PostAndNote => {
            if let Err(e) = append_note(severity, reason) {
                tracing::warn!(error = %e, severity = severity.as_str(), "appending the distress note failed");
            }
        }
        Action::AlreadySuspended => {}
    }
    match std::env::var("SLACK_WEBHOOK_URL") {
        Ok(url) if !url.trim().is_empty() => {
            post(&url, &payload(&Page::from_env(severity, reason, evidence)))
        }
        _ => tracing::warn!("SLACK_WEBHOOK_URL is unset, the distress page was not delivered"),
    }
    Ok(match action {
        Action::PostAndSuspend => {
            r#"{"status":"suspending","hint":"commit your best CANDIDATE.md and end the turn"}"#
                .to_string()
        }
        _ => r#"{"status":"noted"}"#.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{Mutex, MutexGuard};

    /// `FORGE_STORAGE_ROOT` (and the Slack/pod vars) are process-global: the file tests must not
    /// interleave.
    static ENV: Mutex<()> = Mutex::new(());

    fn env_lock() -> MutexGuard<'static, ()> {
        ENV.lock().unwrap_or_else(|e| e.into_inner())
    }

    struct Root {
        dir: std::path::PathBuf,
    }

    impl Root {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("crucible-distress-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            unsafe { std::env::set_var("FORGE_STORAGE_ROOT", &dir) };
            Self { dir }
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
            unsafe { std::env::remove_var("FORGE_STORAGE_ROOT") };
        }
    }

    #[test]
    fn decide_covers_every_severity_and_suspend_state() {
        assert_eq!(decide(Severity::Error, false), Action::PostAndSuspend);
        assert_eq!(decide(Severity::Error, true), Action::AlreadySuspended);
        for sev in [Severity::Info, Severity::Warn] {
            for suspended in [false, true] {
                assert_eq!(
                    decide(sev, suspended),
                    Action::PostAndNote,
                    "{sev:?} never suspends and never no-ops"
                );
            }
        }
    }

    #[test]
    fn an_error_writes_the_marker_and_repeats_are_no_ops() {
        let _g = env_lock();
        let root = Root::new("marker");
        unsafe { std::env::remove_var("SLACK_WEBHOOK_URL") };

        let reply = distress(Severity::Error, "torch skew", &["log-3.txt".to_string()]).unwrap();
        assert!(reply.contains(r#""status":"suspending""#), "{reply}");
        let raw = std::fs::read_to_string(root.dir.join("distress")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["reason"], "torch skew");
        assert_eq!(v["evidence"][0], "log-3.txt");
        assert!(v["ts_ms"].as_u64().unwrap() > 0);
        assert!(v["turn_token"].is_string());
        // No tmp file survives the atomic rename.
        assert!(!root.dir.join("distress.tmp").exists());

        // A second error neither pages nor rewrites.
        let again = distress(Severity::Error, "different reason", &[]).unwrap();
        assert_eq!(again, r#"{"status":"suspended"}"#);
        let after = std::fs::read_to_string(root.dir.join("distress")).unwrap();
        assert_eq!(after, raw, "an idempotent call must not rewrite the marker");
    }

    #[test]
    fn info_and_warn_append_notes_and_never_suspend() {
        let _g = env_lock();
        let root = Root::new("notes");
        unsafe { std::env::remove_var("SLACK_WEBHOOK_URL") };

        assert_eq!(
            distress(Severity::Info, "cache warm", &[]).unwrap(),
            r#"{"status":"noted"}"#
        );
        assert_eq!(
            distress(Severity::Warn, "flaky node", &[]).unwrap(),
            r#"{"status":"noted"}"#
        );
        assert!(
            !root.dir.join("distress").exists(),
            "only error parks the run"
        );
        let lines: Vec<serde_json::Value> =
            std::fs::read_to_string(root.dir.join("distress-notes.jsonl"))
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["severity"], "info");
        assert_eq!(lines[0]["reason"], "cache warm");
        assert_eq!(lines[1]["severity"], "warn");
    }

    #[test]
    fn severity_parses_from_the_wire_token() {
        assert_eq!(
            serde_json::from_str::<Severity>("\"warn\"").unwrap(),
            Severity::Warn
        );
        assert!(serde_json::from_str::<Severity>("\"fatal\"").is_err());
    }

    #[test]
    fn trace_ids_parse_from_w3c_and_reject_garbage() {
        assert_eq!(
            trace_id("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01").as_deref(),
            Some("0af7651916cd43dd8448eb211c80319c")
        );
        assert!(trace_id("").is_none());
        assert!(trace_id("not-a-traceparent").is_none());
        assert!(
            trace_id("00-00000000000000000000000000000000-0000000000000000-01").is_none(),
            "an all-zeros trace id is invalid"
        );
    }

    fn page(severity: Severity, evidence: Vec<String>, trace_url: Option<String>) -> Page {
        Page {
            severity,
            run: "glm52".into(),
            pod: "glm52-loop".into(),
            namespace: "crucible".into(),
            iteration: "7".into(),
            reason: "TORCH_BOX missing".into(),
            evidence,
            trace_url,
        }
    }

    #[test]
    fn payload_colors_and_titles_by_severity() {
        for (sev, color, emoji) in [
            (Severity::Info, "#439FE0", "ℹ️"),
            (Severity::Warn, "#ECB22E", "⚠️"),
            (Severity::Error, "#E01E5A", "🚨"),
        ] {
            let v = payload(&page(sev, vec![], None));
            let att = &v["attachments"][0];
            assert_eq!(att["color"], color);
            let title = att["title"].as_str().unwrap();
            assert!(title.starts_with(emoji), "{title}");
            assert!(title.ends_with("glm52"), "{title}");
        }
    }

    #[test]
    fn payload_carries_the_operator_one_liners_and_fields() {
        let v = payload(&page(Severity::Error, vec![], None));
        let att = &v["attachments"][0];
        let text = att["text"].as_str().unwrap();
        assert!(text.contains("oc exec glm52-loop -n crucible -- rm /var/lib/forge/distress"));
        assert!(text.contains("oc logs glm52-loop -n crucible --tail=100"));
        assert!(text.contains("TORCH_BOX missing"));
        let field = |name: &str| {
            att["fields"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["title"] == name)
                .map(|f| f["value"].as_str().unwrap().to_string())
        };
        assert_eq!(field("run").as_deref(), Some("glm52"));
        assert_eq!(field("pod").as_deref(), Some("glm52-loop"));
        assert_eq!(field("namespace").as_deref(), Some("crucible"));
        assert_eq!(field("iteration").as_deref(), Some("7"));
    }

    #[test]
    fn payload_links_the_trace_only_when_there_is_one() {
        let with = payload(&page(
            Severity::Error,
            vec![],
            Some("https://app.datadoghq.com/apm/trace/abc".into()),
        ));
        assert!(
            with["attachments"][0]["text"]
                .as_str()
                .unwrap()
                .contains("https://app.datadoghq.com/apm/trace/abc")
        );
        let without = payload(&page(Severity::Error, vec![], None));
        assert!(
            !without["attachments"][0]["text"]
                .as_str()
                .unwrap()
                .contains("/apm/trace/")
        );
    }

    #[test]
    fn payload_caps_evidence_items_and_length() {
        let evidence: Vec<String> = (0..9)
            .map(|i| format!("item-{i} {}", "x".repeat(500)))
            .collect();
        let v = payload(&page(Severity::Warn, evidence, None));
        let text = v["attachments"][0]["text"].as_str().unwrap();
        assert_eq!(text.matches("\n• ").count(), MAX_EVIDENCE_ITEMS);
        assert!(text.contains("item-4"));
        assert!(!text.contains("item-5"), "items past the cap are dropped");
        for line in text.lines().filter(|l| l.starts_with("• ")) {
            assert!(line.chars().count() <= MAX_EVIDENCE_CHARS + 2, "{line}");
        }
    }

    /// A real socket, not a mock: assert the exact method/path/body Slack would receive.
    #[test]
    fn post_delivers_the_payload_to_a_real_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(sock.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut len = 0usize;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).unwrap();
            sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                .unwrap();
            (request_line, String::from_utf8(body).unwrap())
        });

        post(
            &format!("http://{addr}/services/T/B/xyz"),
            &payload(&page(Severity::Error, vec!["log-1".into()], None)),
        );

        let (request_line, body) = server.join().unwrap();
        assert!(
            request_line.starts_with("POST /services/T/B/xyz "),
            "{request_line}"
        );
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["attachments"][0]["color"], "#E01E5A");
        assert!(
            v["attachments"][0]["text"]
                .as_str()
                .unwrap()
                .contains("log-1")
        );
    }

    /// Slack down (nothing listening) must not fail the tool: the marker still lands and the
    /// reply is the normal suspending contract.
    #[test]
    fn a_dead_webhook_still_suspends() {
        let _g = env_lock();
        let root = Root::new("dead-webhook");
        // Bind then drop, so the port is (almost certainly) closed and connect refuses fast.
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        unsafe { std::env::set_var("SLACK_WEBHOOK_URL", format!("http://{addr}/hook")) };

        let reply = distress(Severity::Error, "webhook is down", &[]).unwrap();
        unsafe { std::env::remove_var("SLACK_WEBHOOK_URL") };
        assert!(reply.contains(r#""status":"suspending""#), "{reply}");
        assert!(
            root.dir.join("distress").exists(),
            "the park is the load-bearing half"
        );
    }

    /// The inverse of the test above: with the marker unwritable there is no suspend, so claiming
    /// one would send the agent off to wrap up while the engine keeps spending.
    #[test]
    fn an_unwritable_marker_fails_the_tool_instead_of_claiming_a_suspend() {
        let _g = env_lock();
        let root = Root::new("marker-write-fails");
        // A regular file where the marker directory has to be: create_dir_all and the rename both
        // fail, the same way a full or read-only volume fails.
        let blocked = root.dir.join("blocked");
        std::fs::write(&blocked, "not a directory").unwrap();
        // SAFETY: the ENV mutex scopes the mutation to this test.
        unsafe { std::env::set_var("FORGE_STORAGE_ROOT", blocked.join("storage")) };
        // A listener that would record a page, so "posted nothing" is an assertion, not a guess.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        unsafe { std::env::set_var("SLACK_WEBHOOK_URL", format!("http://{addr}/hook")) };

        let err = distress(Severity::Error, "no room on the volume", &[])
            .expect_err("a marker that cannot be written is a tool error");

        unsafe { std::env::remove_var("SLACK_WEBHOOK_URL") };
        assert!(err.contains("NOT suspended"), "{err}");
        listener
            .set_nonblocking(true)
            .expect("poll the listener without blocking");
        assert!(
            listener.accept().is_err(),
            "no page for a suspend that did not happen"
        );
    }
}
