//! In-process remote control bridge for a stream-mode loop.
//!
//! The bridge is intentionally transport-agnostic: it listens on a plain TCP socket,
//! broadcasts new `state/session.jsonl` lines with a monotonic `seq`, and applies
//! inbound NDJSON commands against the live loop process.

use crate::{Paths, STOP};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type Client = Arc<Mutex<TcpStream>>;

/// How many recently-broadcast session lines the bridge retains in memory for replay. A `tail
/// from_seq=N` inside this window is served straight from the ring; anything older is replayed from
/// `session.jsonl` itself (see [`tail_with_file_replay`]), the seq IS the 1-based file line index,
/// so the file is the authoritative pre-ring history and a late joiner gets the whole run.
const HISTORY_CAP: usize = 4096;

/// One session line stamped with its monotonic `seq`, kept in the bridge's replay ring.
#[derive(Clone)]
struct Stamped {
    seq: u64,
    line: String,
}

/// The broadcast hub: a bounded replay ring plus the currently-subscribed client writers, under one
/// lock so a new subscriber's replay-then-live handoff is atomic against the tail thread's
/// broadcasts, every line is delivered exactly once, in order, with no gap at the boundary.
#[derive(Default)]
struct Hub {
    inner: Mutex<HubInner>,
}

#[derive(Default)]
struct HubInner {
    history: VecDeque<Stamped>,
    clients: Vec<Client>,
}

impl Hub {
    /// Stamp-and-broadcast one line: retain it in the replay ring (bounded) and write it to every
    /// live subscriber, dropping any whose socket has closed.
    fn publish(&self, seq: u64, line: String) {
        let Ok(mut g) = self.inner.lock() else {
            return;
        };
        g.history.push_back(Stamped {
            seq,
            line: line.clone(),
        });
        while g.history.len() > HISTORY_CAP {
            g.history.pop_front();
        }
        g.clients.retain(|c| write_line(c, &line));
    }

    /// The newest seq in the ring (0 before anything was published), the file-replay loop's
    /// per-pass target.
    fn newest_seq(&self) -> u64 {
        self.inner
            .lock()
            .map(|g| g.history.back().map(|s| s.seq).unwrap_or(0))
            .unwrap_or(0)
    }

    /// Attempt the atomic file→ring handoff for a client that has already received everything up to
    /// `last` (from the file): if the ring still covers `last + 1` (or nothing was ever published),
    /// replay the retained lines after `last` and subscribe, all under the one lock, so nothing
    /// published concurrently is missed or doubled. Returns false when the ring's oldest has moved
    /// past `last + 1` (the file grew faster than the replay): the caller reads more file first.
    fn try_handoff(&self, writer: &Client, last: u64) -> bool {
        let Ok(mut g) = self.inner.lock() else {
            return true; // poisoned: behave like the plain subscribe arms elsewhere
        };
        let covered = match g.history.front() {
            None => true,
            Some(oldest) => oldest.seq <= last + 1,
        };
        if !covered {
            return false;
        }
        for s in g.history.iter().filter(|s| s.seq > last) {
            let _ = write_line(writer, &s.line);
        }
        g.clients.push(writer.clone());
        true
    }

    /// Drop a client (its reader thread ended / socket closed) so broadcasts stop targeting it.
    fn unsubscribe(&self, writer: &Client) {
        let Ok(mut g) = self.inner.lock() else {
            return;
        };
        g.clients.retain(|c| !Arc::ptr_eq(c, writer));
    }
}

#[derive(Default)]
pub(crate) struct ControlState {
    paused: AtomicBool,
    next_seq: AtomicU64,
    live_max_cost: Mutex<Option<f64>>,
    status: Mutex<StatusSnapshot>,
    /// A pending re-scope: the regime label to re-baseline into, drained by the loop at the next
    /// iteration head. Set when an approved judge-changing grant arrives.
    rescope: Mutex<Option<String>>,
    /// The regime an open approval would grant (attended path): the loop records it from the agent's
    /// pending marker so an operator `approve` keystroke can turn it into a `rescope` without naming
    /// the regime. `None` when there's no approval awaiting an operator.
    pending_regime: Mutex<Option<String>>,
    /// A pending approval was rejected (deny path): the reason, drained by the park as the terminal
    /// "not granted" outcome. Set by the `deny` command (forge denial / operator / policy).
    deny: Mutex<Option<String>>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct StatusSnapshot {
    pub phase: String,
    pub iter: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_score: Option<f64>,
    pub spend: f64,
    pub paused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_cost: Option<f64>,
}

impl Default for StatusSnapshot {
    fn default() -> Self {
        Self {
            phase: "starting".into(),
            iter: 0,
            best_score: None,
            spend: 0.0,
            paused: false,
            max_cost: None,
        }
    }
}

impl ControlState {
    pub(crate) fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub(crate) fn live_max_cost(&self) -> Option<f64> {
        *self.live_max_cost.lock().unwrap()
    }

    /// Drain a pending re-scope request (the new regime label) if one was set since the last
    /// check. The loop calls this at the iteration head and, if `Some`, re-baselines.
    pub(crate) fn take_rescope(&self) -> Option<String> {
        self.rescope.lock().unwrap().take()
    }

    /// Non-consuming peek: is a re-scope pending? The park polls this so the drain
    /// ([`take_rescope`](Self::take_rescope)) stays the single place that re-baselines.
    pub(crate) fn has_rescope(&self) -> bool {
        self.rescope.lock().unwrap().is_some()
    }

    pub(crate) fn set_status(&self, phase: &str, iter: u32, best_score: Option<f64>, spend: f64) {
        let mut status = self.status.lock().unwrap();
        status.phase = phase.to_string();
        status.iter = iter;
        status.best_score = best_score;
        status.spend = spend;
    }

    pub(crate) fn set_spend(&self, spend: f64) {
        self.status.lock().unwrap().spend = spend;
    }

    fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    fn set_live_max_cost(&self, usd: f64) {
        *self.live_max_cost.lock().unwrap() = Some(usd);
    }

    pub(crate) fn set_rescope(&self, regime: String) {
        *self.rescope.lock().unwrap() = Some(regime);
    }

    /// Record the regime an open approval would grant, so an operator `approve` can resolve it.
    /// The loop sets this when it reads the agent's pending-provisioning marker.
    pub(crate) fn set_pending_regime(&self, regime: String) {
        *self.pending_regime.lock().unwrap() = Some(regime);
    }

    /// Resolve a pending approval (attended path): turn the recorded pending regime into a re-scope
    /// the loop's iteration head will drain. Returns the granted regime, or `None` when there is
    /// nothing awaiting approval.
    pub(crate) fn approve_pending(&self) -> Option<String> {
        let regime = self.pending_regime.lock().unwrap().take()?;
        self.set_rescope(regime.clone());
        Some(regime)
    }

    /// Record that a pending approval was rejected (deny path). Clears any recorded pending regime
    /// so a later `approve` can't resurrect a denied ask.
    pub(crate) fn set_deny(&self, reason: String) {
        *self.pending_regime.lock().unwrap() = None;
        *self.deny.lock().unwrap() = Some(reason);
    }

    /// Drain a denial if one arrived (the terminal "not granted" outcome the park waits on).
    pub(crate) fn take_deny(&self) -> Option<String> {
        self.deny.lock().unwrap().take()
    }

    fn snapshot(&self) -> StatusSnapshot {
        let mut status = self.status.lock().unwrap().clone();
        status.paused = self.is_paused();
        status.max_cost = self.live_max_cost();
        status
    }

    /// Assign the next monotonic `seq` and stamp it onto the line, returning both (the seq feeds the
    /// replay ring, the stamped line goes on the wire). `None` for a blank/torn line, the seq is
    /// still consumed so the numbering matches the file's line count.
    fn stamp_session_line(&self, line: &str) -> Option<(u64, String)> {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst) + 1;
        stamp_session_line(line, seq).map(|stamped| (seq, stamped))
    }
}

/// Start a detached TCP bridge thread. The listener binds to all interfaces so it works
/// inside a pod behind `kubectl port-forward`, while still being reachable as localhost
/// for local smoke tests.
pub(crate) fn spawn_bridge(port: u16, paths: Paths) -> Result<Arc<ControlState>> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .with_context(|| format!("binding control port {port}"))?;
    let state = Arc::new(ControlState::default());
    let hub = Arc::new(Hub::default());
    spawn_session_tail(paths.session_log.clone(), state.clone(), hub.clone());

    let accept_state = state.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => serve_client(stream, &paths, accept_state.clone(), hub.clone()),
                Err(_) => thread::sleep(Duration::from_millis(100)),
            }
        }
    });

    Ok(state)
}

fn spawn_session_tail(path: std::path::PathBuf, state: Arc<ControlState>, hub: Arc<Hub>) {
    thread::spawn(move || {
        let mut pos: Option<u64> = None;
        loop {
            if let Ok(mut file) = File::open(&path) {
                let len = file.metadata().map(|m| m.len()).unwrap_or(0);
                // Start from offset 0 (not the live edge): every line the file already holds gets
                // stamped, so seq == the 1-based file line index and file replay can serve history
                // older than the ring.
                let current = match pos {
                    Some(p) if len >= p => p,
                    Some(_) => 0,
                    None => 0,
                };
                let _ = len;
                pos = Some(read_new_lines(&mut file, current, &state, &hub));
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
}

fn read_new_lines(file: &mut File, start: u64, state: &ControlState, hub: &Hub) -> u64 {
    if file.seek(SeekFrom::Start(start)).is_err() {
        return start;
    }
    let mut pos = start;
    let mut reader = BufReader::new(file);
    loop {
        let mut line = String::new();
        let n = match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(n) => n as u64,
            Err(_) => break,
        };
        if !line.ends_with('\n') {
            break;
        }
        pos += n;
        if let Some((seq, stamped)) = state.stamp_session_line(&line) {
            hub.publish(seq, stamped);
        }
    }
    pos
}

/// Write one NDJSON line to a client and flush, reporting whether the socket is still alive (a dead
/// one is dropped from the broadcast set by the caller).
fn write_line(client: &Client, line: &str) -> bool {
    let Ok(mut stream) = client.lock() else {
        return false;
    };
    writeln!(stream, "{line}")
        .and_then(|_| stream.flush())
        .is_ok()
}

/// Serve a `tail from_seq` that may reach back past the in-memory ring: replay the missing range
/// from `session.jsonl` itself (seq == the 1-based file line index, unparseable lines consume their
/// seq silently, the same rule the live stamping applies), then hand off to the ring atomically
/// via [`Hub::try_handoff`]. Loops because the file can grow while a pass streams: each pass reads
/// up to the ring's newest at pass start, and the handoff only succeeds when the ring still covers
/// the next line, so every line is delivered exactly once, in order, with no gap at the boundary.
fn tail_with_file_replay(path: &Path, hub: &Hub, writer: &Client, from_seq: u64) {
    let mut last = from_seq;
    loop {
        if hub.try_handoff(writer, last) {
            return;
        }
        let target = hub.newest_seq();
        if target <= last {
            // The ring is ahead of `last+1`'s coverage yet has nothing newer than `last`, only a
            // transient between a publish and our read. Yield and retry.
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        last = replay_file_range(path, writer, last, target);
    }
}

/// Stream file lines with 1-based index in `(after, upto]` to the client, stamped exactly like the
/// live path. Returns the last index actually reached (== `upto` unless the file is unexpectedly
/// short, then the handoff loop retries from there). Only full (newline-terminated) lines within
/// `upto` exist by construction: the tail thread already stamped them.
fn replay_file_range(path: &Path, writer: &Client, after: u64, upto: u64) -> u64 {
    let Ok(file) = File::open(path) else {
        return upto; // vanished file: fall through to the ring, which is all that's left
    };
    let reader = BufReader::new(file);
    let mut idx: u64 = 0;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        idx += 1;
        if idx > upto {
            break;
        }
        if idx <= after {
            continue;
        }
        if let Some(stamped) = stamp_session_line(&line, idx) {
            let _ = write_line(writer, &stamped);
        }
    }
    idx.min(upto).max(after)
}

fn serve_client(stream: TcpStream, paths: &Paths, state: Arc<ControlState>, hub: Arc<Hub>) {
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let writer = Arc::new(Mutex::new(writer));

    let paths = paths.clone();
    thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let reply = match line {
                // `tail` subscribes this socket to the live broadcast (replaying from `from_seq`),
                // so it's handled here where the writer lives rather than in `apply_command`.
                Ok(line) => match parse_command(&line) {
                    Ok(ControlCommand::Tail { from_seq }) => {
                        tail_with_file_replay(&paths.session_log, &hub, &writer, from_seq);
                        json!({"ok": true, "cmd": "tail", "from_seq": from_seq})
                    }
                    Ok(cmd) => apply_command(cmd, &paths, &state),
                    Err(e) => json!({"ok": false, "error": e}),
                },
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            };
            write_reply(&writer, &reply);
        }
        // The reader loop ends when the client closes the socket: drop it from the broadcast set so
        // publishes stop targeting a dead writer (a tail subscriber that just went away).
        hub.unsubscribe(&writer);
    });
}

fn write_reply(writer: &Client, reply: &Value) {
    let Ok(mut stream) = writer.lock() else {
        return;
    };
    let _ = writeln!(stream, "{reply}");
    let _ = stream.flush();
}

fn apply_command(cmd: ControlCommand, paths: &Paths, state: &ControlState) -> Value {
    match cmd {
        ControlCommand::Tail { from_seq } => {
            // Subscription happens at the socket layer ([`serve_client`]) where the writer lives; a
            // direct caller that reaches here (never the live relay) only gets the ack.
            json!({"ok": true, "cmd": "tail", "from_seq": from_seq})
        }
        ControlCommand::Abort => {
            STOP.store(true, Ordering::SeqCst);
            crate::pid_registry::kill_all();
            json!({"ok": true, "cmd": "abort"})
        }
        ControlCommand::Stop => {
            STOP.store(true, Ordering::SeqCst);
            json!({"ok": true, "cmd": "stop"})
        }
        ControlCommand::Pause => {
            state.pause();
            json!({"ok": true, "cmd": "pause"})
        }
        ControlCommand::Resume => {
            state.resume();
            json!({"ok": true, "cmd": "resume"})
        }
        ControlCommand::Steer { text } => match append_steer(&paths.steer, &text) {
            Ok(()) => json!({"ok": true, "cmd": "steer"}),
            Err(e) => json!({"ok": false, "cmd": "steer", "error": e.to_string()}),
        },
        ControlCommand::SetBudget { usd } => {
            state.set_live_max_cost(usd);
            json!({"ok": true, "cmd": "set-budget", "usd": usd})
        }
        ControlCommand::Rescope { regime } => {
            state.set_rescope(regime.clone());
            json!({"ok": true, "cmd": "rescope", "regime": regime})
        }
        ControlCommand::Approve => match state.approve_pending() {
            Some(regime) => json!({"ok": true, "cmd": "approve", "regime": regime}),
            None => json!({"ok": false, "cmd": "approve", "error": "no pending approval"}),
        },
        ControlCommand::Deny { reason } => {
            state.set_deny(reason.clone());
            json!({"ok": true, "cmd": "deny", "reason": reason})
        }
        ControlCommand::Status => {
            json!({"ok": true, "cmd": "status", "status": state.snapshot()})
        }
    }
}

/// The legacy cross-process stop file external tooling may write.
/// Unknown fields are ignored so older writers that include debugging metadata still work.
#[derive(Deserialize, Default)]
struct StopFile {
    #[serde(default)]
    stop: bool,
}

pub(crate) fn stop_file_says_stop(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<StopFile>(&s).ok())
        .map(|c| c.stop)
        .unwrap_or(false)
}

#[derive(Debug, PartialEq)]
enum ControlCommand {
    Abort,
    Stop,
    Pause,
    Resume,
    Steer {
        text: String,
    },
    SetBudget {
        usd: f64,
    },
    Rescope {
        regime: String,
    },
    Approve,
    Deny {
        reason: String,
    },
    Status,
    /// Subscribe to the live session broadcast, replaying retained lines with `seq > from_seq`
    /// first (resume support for the read-only live relay). `from_seq = 0` = from the ring's oldest.
    Tail {
        from_seq: u64,
    },
}

fn parse_command(line: &str) -> std::result::Result<ControlCommand, String> {
    let value: Value = serde_json::from_str(line.trim()).map_err(|e| e.to_string())?;
    if let Some(cmd) = value.as_str() {
        return command_from_name(cmd, &value);
    }
    let obj = value
        .as_object()
        .ok_or_else(|| "command must be a JSON object or string".to_string())?;
    let cmd = obj
        .get("cmd")
        .or_else(|| obj.get("command"))
        .or_else(|| obj.get("kind"))
        .and_then(Value::as_str)
        .ok_or_else(|| "command object needs cmd".to_string())?;
    command_from_name(cmd, &value)
}

fn command_from_name(cmd: &str, value: &Value) -> std::result::Result<ControlCommand, String> {
    match cmd {
        "abort" => Ok(ControlCommand::Abort),
        "stop" => Ok(ControlCommand::Stop),
        "pause" => Ok(ControlCommand::Pause),
        "resume" => Ok(ControlCommand::Resume),
        "approve" => Ok(ControlCommand::Approve),
        "deny" => {
            // Reason is optional (an operator may just reject); default to a generic note.
            let reason = value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("rejected")
                .trim()
                .to_string();
            Ok(ControlCommand::Deny { reason })
        }
        "status" => Ok(ControlCommand::Status),
        "tail" => {
            // The seq to resume after; absent/0 = replay from the ring's oldest retained line.
            let from_seq = value.get("from_seq").and_then(Value::as_u64).unwrap_or(0);
            Ok(ControlCommand::Tail { from_seq })
        }
        "steer" => {
            let text = value
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| "steer needs text".to_string())?;
            Ok(ControlCommand::Steer {
                text: text.trim().to_string(),
            })
        }
        "set-budget" | "set_budget" => {
            let usd = value
                .get("usd")
                .and_then(Value::as_f64)
                .ok_or_else(|| "set-budget needs numeric usd".to_string())?;
            if !usd.is_finite() || usd < 0.0 {
                return Err("set-budget usd must be a finite non-negative number".into());
            }
            Ok(ControlCommand::SetBudget { usd })
        }
        "rescope" | "re-scope" => {
            // A judge-changing grant approved out-of-band: re-baseline into this regime.
            let regime = value
                .get("regime")
                .and_then(Value::as_str)
                .unwrap_or("rescoped")
                .trim()
                .to_string();
            Ok(ControlCommand::Rescope { regime })
        }
        other => Err(format!("unknown control command {other:?}")),
    }
}

/// Append one steer payload to `path` in the marker-wrapped shape `loop_driver::take_steer` expects.
/// `pub(crate)` because `pr_watch`'s reseed sink writes the exact same shape straight to a file when
/// there's no live run's control bridge to send a `steer` command to.
pub(crate) fn append_steer(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let payload = format!(
        "<!-- steer @{} by control -->\n{}\n",
        now_secs(),
        text.trim()
    );
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(payload.as_bytes())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn stamp_session_line(line: &str, seq: u64) -> Option<String> {
    let mut value: Value = serde_json::from_str(line.trim()).ok()?;
    let Value::Object(obj) = &mut value else {
        return None;
    };
    obj.insert("seq".into(), Value::from(seq));
    serde_json::to_string(&value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_control_commands() {
        assert_eq!(
            parse_command(r#"{"cmd":"abort"}"#).unwrap(),
            ControlCommand::Abort
        );
        assert_eq!(parse_command(r#""stop""#).unwrap(), ControlCommand::Stop);
        assert_eq!(
            parse_command(r#"{"kind":"steer","text":" try cache-first "}"#).unwrap(),
            ControlCommand::Steer {
                text: "try cache-first".into()
            }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"set-budget","usd":2.5}"#).unwrap(),
            ControlCommand::SetBudget { usd: 2.5 }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"rescope","regime":"concurrency=48"}"#).unwrap(),
            ControlCommand::Rescope {
                regime: "concurrency=48".into()
            }
        );
        assert!(parse_command(r#"{"cmd":"set-budget","usd":-1}"#).is_err());
        assert!(parse_command(r#"{"cmd":"steer"}"#).is_err());
    }

    #[test]
    fn stamps_session_lines_with_monotonic_seq() {
        let state = ControlState::default();
        let (seq_one, one) = state
            .stamp_session_line(r#"{"v":1,"kind":"note","msg":"a"}"#)
            .unwrap();
        let (seq_two, two) = state
            .stamp_session_line(r#"{"v":1,"kind":"note","msg":"b"}"#)
            .unwrap();
        assert_eq!(seq_one, 1);
        assert_eq!(seq_two, 2);
        let one: Value = serde_json::from_str(&one).unwrap();
        let two: Value = serde_json::from_str(&two).unwrap();
        assert_eq!(one["seq"], 1);
        assert_eq!(two["seq"], 2);
        assert_eq!(one["kind"], "note");
        assert_eq!(two["msg"], "b");
    }

    #[test]
    fn set_budget_zero_is_a_live_unlimited_cap() {
        let state = ControlState::default();
        state.set_live_max_cost(0.0);
        assert_eq!(state.live_max_cost(), Some(0.0));
    }

    #[test]
    fn approve_turns_a_pending_regime_into_a_rescope() {
        let state = ControlState::default();
        // Nothing pending yet: approve is a no-op.
        assert_eq!(state.approve_pending(), None);
        assert!(!state.has_rescope());

        // The loop records the regime an approval would grant; an operator `approve` resolves it.
        state.set_pending_regime("concurrency=48".into());
        assert_eq!(state.approve_pending().as_deref(), Some("concurrency=48"));
        assert!(
            state.has_rescope(),
            "approve sets the re-scope the loop drains"
        );
        assert_eq!(state.take_rescope().as_deref(), Some("concurrency=48"));
        // Consumed: a second approve has nothing to grant.
        assert_eq!(state.approve_pending(), None);
    }

    #[test]
    fn parses_approve_command() {
        assert_eq!(
            parse_command(r#"{"cmd":"approve"}"#).unwrap(),
            ControlCommand::Approve
        );
        assert_eq!(
            parse_command(r#""approve""#).unwrap(),
            ControlCommand::Approve
        );
    }

    #[test]
    fn parses_deny_command_with_optional_reason() {
        assert_eq!(
            parse_command(r#"{"cmd":"deny","reason":"over budget"}"#).unwrap(),
            ControlCommand::Deny {
                reason: "over budget".into()
            }
        );
        // Bare deny defaults its reason.
        assert_eq!(
            parse_command(r#""deny""#).unwrap(),
            ControlCommand::Deny {
                reason: "rejected".into()
            }
        );
    }

    #[test]
    fn deny_drops_a_recorded_pending_regime_so_approve_cant_resurrect_it() {
        let state = ControlState::default();
        state.set_pending_regime("concurrency=48".into());
        state.set_deny("policy says no".into());
        assert_eq!(state.take_deny().as_deref(), Some("policy says no"));
        // The pending regime is gone, so a stray `approve` grants nothing.
        assert_eq!(state.approve_pending(), None);
        assert!(!state.has_rescope());
    }

    #[test]
    fn stop_file_accepts_old_metadata_and_minimal_shape() {
        let path =
            std::env::temp_dir().join(format!("crucible-stop-file-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"stop":true,"kill_agent":true,"seq":42}"#).unwrap();
        assert!(stop_file_says_stop(&path));

        std::fs::write(&path, r#"{"stop":true}"#).unwrap();
        assert!(stop_file_says_stop(&path));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ignores_blank_or_torn_session_lines() {
        assert!(stamp_session_line("", 1).is_none());
        assert!(stamp_session_line(r#"{"v":1,"kind":"note""#, 1).is_none());
    }

    #[test]
    fn parses_tail_command_with_and_without_from_seq() {
        assert_eq!(
            parse_command(r#"{"cmd":"tail","from_seq":42}"#).unwrap(),
            ControlCommand::Tail { from_seq: 42 }
        );
        // Absent from_seq defaults to 0 (replay from the ring's oldest).
        assert_eq!(
            parse_command(r#""tail""#).unwrap(),
            ControlCommand::Tail { from_seq: 0 }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"tail","from_seq":7}"#).unwrap(),
            ControlCommand::Tail { from_seq: 7 }
        );
    }

    /// Read one NDJSON line from a stream with a bounded timeout, returning its parsed `seq`.
    fn read_seq(reader: &mut BufReader<TcpStream>) -> i64 {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read line");
        let v: Value = serde_json::from_str(line.trim()).expect("json");
        v["seq"].as_i64().expect("seq")
    }

    #[test]
    fn hub_replays_from_seq_then_delivers_live_exactly_once_in_order() {
        // A real socket pair stands in for a subscribed client; the Hub is the actual replay core.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let hub = Arc::new(Hub::default());

        // Three lines already broadcast (retained in the ring; no subscriber yet).
        hub.publish(1, r#"{"seq":1,"m":"a"}"#.to_string());
        hub.publish(2, r#"{"seq":2,"m":"b"}"#.to_string());
        hub.publish(3, r#"{"seq":3,"m":"c"}"#.to_string());

        let client = TcpStream::connect(addr).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("timeout");
        let (server_side, _) = listener.accept().expect("accept");
        let writer = Arc::new(Mutex::new(server_side));

        // Resume after seq 1 (ring covers 2): the handoff replays 2 and 3 and subscribes...
        assert!(hub.try_handoff(&writer, 1), "ring covers last+1");
        // ...then a live line arrives after subscription.
        hub.publish(4, r#"{"seq":4,"m":"d"}"#.to_string());

        let mut reader = BufReader::new(client);
        assert_eq!(read_seq(&mut reader), 2, "replayed, seq 1 skipped");
        assert_eq!(read_seq(&mut reader), 3, "replayed in order");
        assert_eq!(read_seq(&mut reader), 4, "live after the replay seam");
    }

    #[test]
    fn bridge_tail_command_streams_session_lines_over_tcp() {
        // A full bridge over a real socket, tailing a real session.jsonl. Prove the `tail` command
        // subscribes and streams stamped lines end to end.
        let root =
            std::env::temp_dir().join(format!("crucible-bridge-tail-{}", std::process::id()));
        let state_dir = root.join("state");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let session_log = state_dir.join("session.jsonl");
        // The bridge tails from the file's current end, so any pre-existing content is not replayed;
        // start it empty and append after it's up (matching the real loop, which writes as it runs).
        std::fs::write(&session_log, "").expect("seed empty session log");

        let paths = Paths {
            workspace: root.clone(),
            skills: None,
            steer: root.join("STEER.md"),
            state: state_dir.clone(),
            session_log: session_log.clone(),
            control: state_dir.join("control.json"),
            escalation: root.join("ESCALATION.json"),
            provisioning: root.join("PROVISIONING_PENDING.json"),
        };

        let port = {
            let l = TcpListener::bind("127.0.0.1:0").expect("free port");
            l.local_addr().expect("addr").port()
        };
        spawn_bridge(port, paths).expect("spawn bridge");
        // Let the tail thread reach the (empty) file's end before we start appending.
        thread::sleep(Duration::from_millis(200));

        let append = |m: &str| {
            let mut f = OpenOptions::new()
                .append(true)
                .open(&session_log)
                .expect("reopen session log");
            writeln!(f, r#"{{"kind":"note","m":"{m}"}}"#).expect("append");
        };

        // Two lines land while the bridge is up (no subscriber yet): they get seq 1 and 2 into the
        // replay ring. Give the 100ms poll room to see both.
        append("one");
        append("two");
        thread::sleep(Duration::from_millis(300));

        let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("timeout");
        let mut writer = stream.try_clone().expect("clone");
        // Resume after seq 1: expect line 2 replayed, then a newly appended line live.
        writeln!(writer, r#"{{"cmd":"tail","from_seq":1}}"#).expect("send tail");
        writer.flush().expect("flush");

        // The subscribe replays before the command loop writes the `tail` ack, so the two frame
        // kinds (session lines carrying `seq`, the ack carrying `cmd`) can interleave, classify by
        // content, exactly as the live relay does, rather than assuming an order.
        let mut reader = BufReader::new(stream);
        let mut got_ack = false;
        let next_seq = |reader: &mut BufReader<TcpStream>, got_ack: &mut bool| -> i64 {
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read frame");
                let v: Value = serde_json::from_str(line.trim()).expect("json");
                if v.get("cmd").is_some() {
                    *got_ack = true;
                    continue;
                }
                return v["seq"].as_i64().expect("seq");
            }
        };

        // The replayed line 2 (seq 1 skipped by from_seq=1).
        assert_eq!(
            next_seq(&mut reader, &mut got_ack),
            2,
            "replayed after from_seq"
        );

        // A third line appended now is broadcast live to the subscriber.
        append("three");
        assert_eq!(
            next_seq(&mut reader, &mut got_ack),
            3,
            "live-streamed appended line"
        );
        assert!(got_ack, "the tail command was acked");

        let _ = std::fs::remove_dir_all(&root);
    }

    fn tempdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crucible-control-{name}-{}-{}",
            std::process::id(),
            now_secs()
        ));
        std::fs::create_dir_all(&dir).expect("tempdir");
        dir
    }

    fn socket_pair() -> (BufReader<TcpStream>, Client) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let client = TcpStream::connect(addr).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let (server_side, _) = listener.accept().expect("accept");
        (BufReader::new(client), Arc::new(Mutex::new(server_side)))
    }

    #[test]
    fn try_handoff_declines_when_the_ring_aged_past_the_resume_point() {
        let hub = Hub::default();
        // Ring holds 100..=102, a client that has only seen up to 42 has a gap (43..=99 aged out).
        for seq in 100..=102u64 {
            hub.publish(seq, format!(r#"{{"seq":{seq}}}"#));
        }
        let (_reader, writer) = socket_pair();
        assert!(
            !hub.try_handoff(&writer, 42),
            "gap: must read the file first"
        );
        assert!(
            hub.try_handoff(&writer, 99),
            "contiguous: 100 is the next line"
        );
    }

    #[test]
    fn file_replay_streams_history_older_than_the_ring_then_hands_off() {
        // 60 session lines on disk; the ring only retains the newest 3 (publishes 58..=60), a
        // tail from 0 must get 1..=60 exactly once, in order, then live lines.
        let dir = tempdir("file-replay");
        let path = dir.join("session.jsonl");
        let mut body = String::new();
        for i in 1..=60u64 {
            body.push_str(&format!("{{\"v\":1,\"kind\":\"note\",\"msg\":\"m{i}\"}}\n"));
        }
        std::fs::write(&path, body).expect("write");

        let hub = Arc::new(Hub::default());
        for seq in 58..=60u64 {
            hub.publish(seq, format!(r#"{{"seq":{seq},"live":true}}"#));
        }
        // Force the gap: drop everything older than 58 out of the ring window.
        {
            let mut g = hub.inner.lock().unwrap();
            while g.history.len() > 3 {
                g.history.pop_front();
            }
        }

        let (mut reader, writer) = socket_pair();
        tail_with_file_replay(&path, &hub, &writer, 0);
        hub.publish(61, r#"{"seq":61,"live":true}"#.to_string());

        for expect in 1..=61i64 {
            assert_eq!(read_seq(&mut reader), expect, "in order, exactly once");
        }
    }

    #[test]
    fn file_replay_skips_unparseable_lines_but_their_seq_is_consumed() {
        let dir = tempdir("torn-line");
        let path = dir.join("session.jsonl");
        // Line 2 is torn garbage: its seq is consumed (numbering = file line index) but not sent.
        std::fs::write(
            &path,
            "{\"v\":1,\"kind\":\"note\",\"msg\":\"a\"}\n{\"broken\n{\"v\":1,\"kind\":\"note\",\"msg\":\"c\"}\n",
        )
        .expect("write");
        // Ring holds only seq 3 (oldest = 3 > 0+1), so the tail must take the file pass.
        let hub = Hub::default();
        hub.publish(3, r#"{"seq":3,"m":"c"}"#.to_string());
        let (mut reader, writer) = socket_pair();
        tail_with_file_replay(&path, &hub, &writer, 0);
        assert_eq!(read_seq(&mut reader), 1);
        assert_eq!(
            read_seq(&mut reader),
            3,
            "seq 2 consumed by the torn line, never sent"
        );
    }

    #[test]
    fn file_replay_converges_while_the_file_keeps_growing() {
        // The race the handoff loop exists for: lines keep arriving while the file pass streams.
        // A writer thread appends + publishes 20 more lines while the replay runs; the client must
        // still see 1..=70 exactly once in order.
        let dir = tempdir("growing");
        let path = dir.join("session.jsonl");
        let mut body = String::new();
        for i in 1..=50u64 {
            body.push_str(&format!("{{\"v\":1,\"kind\":\"note\",\"msg\":\"m{i}\"}}\n"));
        }
        std::fs::write(&path, body).expect("write");

        let hub = Arc::new(Hub::default());
        hub.publish(50, r#"{"seq":50}"#.to_string());
        {
            let mut g = hub.inner.lock().unwrap();
            while g.history.len() > 1 {
                g.history.pop_front();
            }
        }

        let grower = {
            let hub = hub.clone();
            let path = path.clone();
            thread::spawn(move || {
                for i in 51..=70u64 {
                    let line = format!("{{\"v\":1,\"kind\":\"note\",\"msg\":\"m{i}\"}}\n");
                    let mut f = OpenOptions::new().append(true).open(&path).expect("append");
                    f.write_all(line.as_bytes()).expect("grow");
                    hub.publish(i, format!(r#"{{"seq":{i}}}"#));
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };

        let (mut reader, writer) = socket_pair();
        tail_with_file_replay(&path, &hub, &writer, 0);
        grower.join().expect("grower");
        // Anything not yet delivered at handoff arrives live; read all 70 in order.
        for expect in 1..=70i64 {
            assert_eq!(read_seq(&mut reader), expect, "exactly once, in order");
        }
    }
}
