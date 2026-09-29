//! Live progress lines for a playbook agent turn: one compact line per event the harness parses,
//! throttled where the stream is a firehose, and a heartbeat when the model goes quiet.

use crate::agent::event::AgentEvent;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const LINE_CHARS: usize = 160;
const DELTA_EVERY: Duration = Duration::from_secs(30);
const TOKENS_EVERY: Duration = Duration::from_secs(60);
const QUIET_AFTER: Duration = Duration::from_secs(60);

/// What a turn has said so far, folded into lines worth printing.
#[derive(Debug)]
pub struct TurnLog {
    started: Instant,
    last_event_at: Instant,
    last_line: Option<String>,
    last_beat_at: Option<Instant>,
    tokens_at: Option<Instant>,
    thinking: Pending,
    text: Pending,
}

/// Streamed deltas not yet reported.
#[derive(Debug, Default)]
struct Pending {
    buf: String,
    flushed_at: Option<Instant>,
}

impl TurnLog {
    pub fn new(now: Instant) -> Self {
        Self {
            started: now,
            last_event_at: now,
            last_line: None,
            last_beat_at: None,
            tokens_at: None,
            thinking: Pending::default(),
            text: Pending::default(),
        }
    }

    /// The lines one event produces, oldest first. Deltas accumulate and flush every
    /// [`DELTA_EVERY`] or when a different kind of event arrives; token samples print at most
    /// every [`TOKENS_EVERY`].
    pub fn on_event(&mut self, ev: &AgentEvent, now: Instant) -> Vec<String> {
        self.last_event_at = now;
        self.last_beat_at = None;
        let mut lines = Vec::new();
        match ev {
            AgentEvent::Thinking { delta } => {
                self.thinking.buf.push_str(delta);
                if now.duration_since(self.thinking.flushed_at.unwrap_or(self.started))
                    >= DELTA_EVERY
                {
                    lines.extend(self.flush_thinking(now));
                }
            }
            AgentEvent::Text { delta } => {
                lines.extend(self.flush_thinking(now));
                self.text.buf.push_str(delta);
                if now.duration_since(self.text.flushed_at.unwrap_or(self.started)) >= DELTA_EVERY {
                    lines.extend(self.flush_text(now));
                }
            }
            AgentEvent::Tokens(_) => {
                if self
                    .tokens_at
                    .is_none_or(|at| now.duration_since(at) >= TOKENS_EVERY)
                {
                    self.tokens_at = Some(now);
                    lines.extend(crate::agent::turn::human_line(ev));
                }
            }
            AgentEvent::Result {
                subtype,
                is_error,
                turns,
                cost_usd,
                error,
            } => {
                lines.extend(self.flush_all(now));
                let mut line = format!("result {subtype} turns={turns} cost=${cost_usd:.4}");
                if *is_error {
                    line.push_str(&format!(
                        " error: {}",
                        error.as_deref().unwrap_or("unspecified")
                    ));
                }
                lines.push(line);
            }
            AgentEvent::Error { .. } => {
                lines.extend(self.flush_all(now));
                lines.extend(crate::agent::turn::human_line(ev));
            }
            other => {
                if let Some(line) = crate::agent::turn::human_line(other) {
                    lines.extend(self.flush_all(now));
                    lines.push(compact(&line));
                }
            }
        }
        if let Some(last) = lines.last() {
            self.last_line = Some(last.clone());
        }
        lines
    }

    /// A line when the model has been quiet for [`QUIET_AFTER`], repeated at most that often.
    pub fn heartbeat(&mut self, now: Instant) -> Option<String> {
        let since = self.last_beat_at.unwrap_or(self.last_event_at);
        if now.duration_since(since) < QUIET_AFTER {
            return None;
        }
        self.last_beat_at = Some(now);
        let quiet = friendly(now.duration_since(self.last_event_at));
        let turn = friendly(now.duration_since(self.started));
        Some(match &self.last_line {
            Some(last) => format!("waiting on the model {quiet} (turn {turn}; last: {last})"),
            None => format!("waiting on the model {quiet} (no event yet)"),
        })
    }

    fn flush_all(&mut self, now: Instant) -> Vec<String> {
        let mut lines: Vec<String> = self.flush_thinking(now).into_iter().collect();
        lines.extend(self.flush_text(now));
        lines
    }

    fn flush_thinking(&mut self, now: Instant) -> Option<String> {
        let chars = std::mem::take(&mut self.thinking.buf).chars().count();
        self.thinking.flushed_at = Some(now);
        (chars > 0).then(|| format!("\u{1f9e0} thinking\u{2026} {chars} chars"))
    }

    fn flush_text(&mut self, now: Instant) -> Option<String> {
        let text = std::mem::take(&mut self.text.buf);
        self.text.flushed_at = Some(now);
        text.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(compact)
    }
}

/// `d` to the whole second, as jiff's friendly format: `7m 7s`.
fn friendly(d: Duration) -> String {
    let secs = i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
    format!("{:#}", jiff::SignedDuration::from_secs(secs))
}

/// The first line of `s`, cut to [`LINE_CHARS`] characters.
fn compact(s: &str) -> String {
    let first = s.lines().next().unwrap_or_default().trim_end();
    match first.char_indices().nth(LINE_CHARS) {
        Some((cut, _)) => format!("{}\u{2026}", &first[..cut]),
        None => first.to_string(),
    }
}

/// Prints [`TurnLog::heartbeat`] lines on its own thread while a turn blocks the caller's.
/// Dropping it stops and joins the thread.
pub struct Heartbeat {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Heartbeat {
    pub fn spawn(log: Arc<Mutex<TurnLog>>, emit: impl Fn(String) + Send + 'static) -> Self {
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            loop {
                match stopped.recv_timeout(Duration::from_secs(5)) {
                    Err(RecvTimeoutError::Timeout) => {
                        let line = log
                            .lock()
                            .ok()
                            .and_then(|mut l| l.heartbeat(Instant::now()));
                        if let Some(line) = line {
                            emit(line);
                        }
                    }
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        Self {
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::event::{AgentEvent, Tokens};
    use crate::plan::turn_log::*;

    fn at(start: Instant, secs: u64) -> Instant {
        start + Duration::from_secs(secs)
    }

    fn thinking(s: &str) -> AgentEvent {
        AgentEvent::Thinking {
            delta: s.to_string(),
        }
    }

    fn text(s: &str) -> AgentEvent {
        AgentEvent::Text {
            delta: s.to_string(),
        }
    }

    fn tokens(output: u64) -> AgentEvent {
        AgentEvent::Tokens(Tokens {
            output,
            ..Tokens::default()
        })
    }

    #[test]
    fn a_tool_call_prints_at_once_and_flushes_the_thinking_before_it() {
        let t0 = Instant::now();
        let mut log = TurnLog::new(t0);
        assert!(
            log.on_event(&thinking("let me look at"), at(t0, 1))
                .is_empty()
        );
        assert!(
            log.on_event(&thinking(" the validator"), at(t0, 2))
                .is_empty()
        );
        let lines = log.on_event(
            &AgentEvent::Tool {
                name: "Grep".to_string(),
                summary: "validate_autoresearch crucible/src".to_string(),
                subagent: false,
                input: None,
                result: None,
            },
            at(t0, 3),
        );
        assert_eq!(
            lines,
            vec![
                "\u{1f9e0} thinking\u{2026} 28 chars".to_string(),
                "\u{1f527} Grep validate_autoresearch crucible/src".to_string(),
            ]
        );
    }

    #[test]
    fn long_thinking_reports_progress_every_thirty_seconds() {
        let t0 = Instant::now();
        let mut log = TurnLog::new(t0);
        assert!(log.on_event(&thinking("abc"), at(t0, 10)).is_empty());
        assert_eq!(
            log.on_event(&thinking("de"), at(t0, 30)),
            vec!["\u{1f9e0} thinking\u{2026} 5 chars".to_string()]
        );
        assert!(log.on_event(&thinking("f"), at(t0, 45)).is_empty());
        assert_eq!(
            log.on_event(&thinking("g"), at(t0, 61)),
            vec!["\u{1f9e0} thinking\u{2026} 2 chars".to_string()]
        );
    }

    #[test]
    fn text_reports_its_first_line_cut_to_a_readable_width() {
        let t0 = Instant::now();
        let mut log = TurnLog::new(t0);
        let long = "x".repeat(400);
        assert!(log.on_event(&text("\n\n"), at(t0, 1)).is_empty());
        assert!(log.on_event(&text(&long), at(t0, 2)).is_empty());
        let lines = log.on_event(
            &AgentEvent::Error {
                error_type: "api".to_string(),
                message: "overloaded".to_string(),
            },
            at(t0, 3),
        );
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0].chars().count(), 161);
        assert!(lines[0].ends_with('\u{2026}'));
        assert_eq!(lines[1], "\u{274c} Error: api: overloaded");
    }

    #[test]
    fn retries_are_never_throttled() {
        let t0 = Instant::now();
        let mut log = TurnLog::new(t0);
        for n in 1..=3 {
            let lines = log.on_event(
                &AgentEvent::Retry {
                    attempt: n,
                    max: 10,
                    error: "529 overloaded".to_string(),
                },
                at(t0, u64::from(n)),
            );
            assert_eq!(
                lines,
                vec![format!("\u{1f504} Retry {n}/10 529 overloaded")]
            );
        }
    }

    #[test]
    fn token_samples_print_at_most_once_a_minute() {
        let t0 = Instant::now();
        let mut log = TurnLog::new(t0);
        assert_eq!(log.on_event(&tokens(10), at(t0, 1)).len(), 1);
        assert!(log.on_event(&tokens(20), at(t0, 30)).is_empty());
        assert_eq!(log.on_event(&tokens(30), at(t0, 61)).len(), 1);
    }

    #[test]
    fn the_result_flushes_pending_text_and_names_an_error() {
        let t0 = Instant::now();
        let mut log = TurnLog::new(t0);
        assert!(log.on_event(&text("done here"), at(t0, 1)).is_empty());
        let lines = log.on_event(
            &AgentEvent::Result {
                subtype: "error_max_turns".to_string(),
                is_error: true,
                turns: 40,
                cost_usd: 0.5,
                error: None,
            },
            at(t0, 2),
        );
        assert_eq!(
            lines,
            vec![
                "done here".to_string(),
                "result error_max_turns turns=40 cost=$0.5000 error: unspecified".to_string(),
            ]
        );
    }

    #[test]
    fn the_heartbeat_speaks_after_a_minute_of_quiet_and_then_once_a_minute() {
        let t0 = Instant::now();
        let mut log = TurnLog::new(t0);
        assert_eq!(log.heartbeat(at(t0, 59)), None);
        assert_eq!(
            log.heartbeat(at(t0, 60)).as_deref(),
            Some("waiting on the model 1m (no event yet)")
        );
        assert_eq!(log.heartbeat(at(t0, 90)), None, "not twice in a minute");
        assert!(log.heartbeat(at(t0, 120)).is_some());

        log.on_event(
            &AgentEvent::Tool {
                name: "Read".to_string(),
                summary: "labels.json".to_string(),
                subagent: false,
                input: None,
                result: None,
            },
            at(t0, 130),
        );
        assert_eq!(
            log.heartbeat(at(t0, 150)),
            None,
            "an event resets the quiet"
        );
        assert_eq!(
            log.heartbeat(at(t0, 190)).as_deref(),
            Some("waiting on the model 1m (turn 3m 10s; last: \u{1f527} Read labels.json)")
        );
    }

    #[test]
    fn a_real_claude_capture_reads_as_text_tokens_and_a_result() {
        let fixture =
            include_str!("../../../crucible-harness/src/testdata/claude_stream_hello.jsonl");
        let mut parser = crucible_harness::stream_json::StreamJsonParser::default();
        let t0 = Instant::now();
        let mut log = TurnLog::new(t0);
        let lines: Vec<String> = fixture
            .lines()
            .flat_map(|l| parser.push(l))
            .flat_map(|ev| log.on_event(&ev, at(t0, 1)))
            .collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].starts_with("\u{1f4ca} TOKENS"), "{lines:?}");
        assert_eq!(lines[1], "hello");
        assert!(lines[2].starts_with("result success turns="), "{lines:?}");
    }

    #[test]
    fn the_heartbeat_thread_stops_when_dropped() {
        let log = Arc::new(Mutex::new(TurnLog::new(Instant::now())));
        let beat = Heartbeat::spawn(log, |_| {});
        let started = Instant::now();
        drop(beat);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "drop joins promptly"
        );
    }
}
