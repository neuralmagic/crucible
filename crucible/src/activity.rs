//! The bounded within-turn activity feed: the stdout ticker that keeps a long agent turn from
//! looking like a hung pod. Shared by `scope`'s propose/adversary rounds and the grounded ranking
//! turn; each names its own marker prefix so a log consumer can tell the two apart.

use crate::event::{AgentEvent, RawStream, cost_of, estimate_cost};
use serde::Serialize;

/// One activity line's payload: a live-view hint, never a result.
#[derive(Debug, Clone, Serialize)]
pub struct ActivityLine {
    /// `tool` | `text` | `usage` | `stage` | `stderr` | `truncated`.
    pub kind: &'static str,
    /// The tool name, `tool` lines only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A capped human-readable summary (tool input, text snippet, token count, stage banner,
    /// stderr line).
    pub detail: String,
    /// Total cost (USD) observed so far: turns already finished + the live turn's samples.
    pub cost_so_far: f64,
}

/// Caps on one activity line's fields, a hint for a ticker, never a transcript.
pub(crate) const ACTIVITY_TOOL_CAP: usize = 80;
pub(crate) const ACTIVITY_TEXT_CAP: usize = 120;

/// At most one non-tool activity line (text/usage) per this interval. A human-watchable ticker
/// needs ~1 update per few seconds; tool calls are API-round-trip paced already and emit freely.
pub(crate) const ACTIVITY_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Total activity bytes one run may emit. The kubelet's 10 MiB log budget is shared with
/// the transcript (8 MiB pre-gzip cap), the pack (4 MiB pre-gzip cap), and the report marker;
/// 256 KiB of activity is noise against that headroom while still covering hours of ticking
/// (~200 bytes/line × one line per 5s ≈ 144 KiB over an hour).
const ACTIVITY_BYTE_BUDGET: usize = 256 * 1024;

/// The bounded within-turn activity feed: turns the agent events a turn sink otherwise swallows
/// into `<marker> {json}` stdout lines. Three bounds, in order: the `--marker` gate (disabled =
/// fully silent), the per-line char caps + text/usage rate limit, and the total byte budget
/// (exhausted = one honest `truncated` line, then silence).
pub struct ActivityFeed {
    marker: &'static str,
    enabled: bool,
    pub(crate) bytes_left: usize,
    truncated: bool,
    last_slow: Option<std::time::Instant>,
    /// Cost accumulated by rounds that already finished.
    base_cost: f64,
    /// The live turn's latest cumulative cost sample (authoritative if reported, else estimated).
    turn_cost: f64,
}

impl ActivityFeed {
    /// A feed emitting `marker`-prefixed lines (`SCOPE_ACTIVITY_MARKER` for `scope`,
    /// `RANK_ACTIVITY_MARKER` for the grounded ranking turn).
    pub fn new(marker: &'static str, enabled: bool) -> Self {
        ActivityFeed {
            marker,
            enabled,
            bytes_left: ACTIVITY_BYTE_BUDGET,
            truncated: false,
            last_slow: None,
            base_cost: 0.0,
            turn_cost: 0.0,
        }
    }

    /// Rebase at a turn boundary: prior turns' total becomes the floor the live samples add to.
    pub(crate) fn begin_turn(&mut self, cost_so_far: f64) {
        self.base_cost = cost_so_far;
        self.turn_cost = 0.0;
    }

    /// Observe one streamed event; print its activity line if it earns one.
    pub(crate) fn observe(&mut self, model: &str, ev: &AgentEvent) {
        if let Some(line) = self.line_for(model, ev, std::time::Instant::now()) {
            println!("{line}");
        }
    }

    /// The marker line `ev` earns at `now`, if any. Split from [`Self::observe`] so tests drive
    /// the rate limit and budget with controlled instants instead of capturing stdout.
    pub(crate) fn line_for(
        &mut self,
        model: &str,
        ev: &AgentEvent,
        now: std::time::Instant,
    ) -> Option<String> {
        if !self.enabled || self.truncated {
            return None;
        }
        // Cost first: every sample updates the running number, whether or not a line is emitted.
        // max() keeps the readout monotonic within a turn if an estimate lands after an
        // authoritative sample.
        match cost_of(ev) {
            Some(c) => self.turn_cost = self.turn_cost.max(c),
            None => {
                if let AgentEvent::Tokens(t) = ev {
                    self.turn_cost = self.turn_cost.max(estimate_cost(model, t));
                }
            }
        }
        let activity = match ev {
            AgentEvent::Tool { name, summary, .. } => ActivityLine {
                kind: "tool",
                name: Some(name.clone()),
                detail: cap_chars(summary, ACTIVITY_TOOL_CAP),
                cost_so_far: self.cost_so_far(),
            },
            AgentEvent::Text { delta } if !delta.trim().is_empty() => {
                if !self.slow_lane_open(now) {
                    return None;
                }
                ActivityLine {
                    kind: "text",
                    name: None,
                    detail: cap_chars(delta.trim(), ACTIVITY_TEXT_CAP),
                    cost_so_far: self.cost_so_far(),
                }
            }
            AgentEvent::Tokens(t) => {
                if !self.slow_lane_open(now) {
                    return None;
                }
                ActivityLine {
                    kind: "usage",
                    name: None,
                    detail: format!("{} tokens", t.total),
                    cost_so_far: self.cost_so_far(),
                }
            }
            // The openshell driver's orchestration banners (sandbox create / image pull / uploads).
            AgentEvent::Log { level, value, .. } if level == "stage" => ActivityLine {
                kind: "stage",
                name: None,
                detail: cap_chars(value.as_deref().unwrap_or_default(), ACTIVITY_TEXT_CAP),
                cost_so_far: self.cost_so_far(),
            },
            // The agent's own stderr, replayed after it exits: the diagnostic a dying agent
            // prints on its way out.
            AgentEvent::Raw {
                text,
                stream: RawStream::Stderr,
            } if !text.trim().is_empty() => ActivityLine {
                kind: "stderr",
                name: None,
                detail: cap_chars(text.trim(), ACTIVITY_TEXT_CAP),
                cost_so_far: self.cost_so_far(),
            },
            _ => return None,
        };
        let line = format!(
            "{} {}",
            self.marker,
            serde_json::to_string(&activity).unwrap_or_else(|_| "{}".to_string())
        );
        if line.len() + 1 > self.bytes_left {
            self.truncated = true;
            let tail = ActivityLine {
                kind: "truncated",
                name: None,
                detail: format!(
                    "activity feed truncated ({ACTIVITY_BYTE_BUDGET} byte budget spent)"
                ),
                cost_so_far: self.cost_so_far(),
            };
            return Some(format!(
                "{} {}",
                self.marker,
                serde_json::to_string(&tail).unwrap_or_else(|_| "{}".to_string())
            ));
        }
        self.bytes_left -= line.len() + 1;
        Some(line)
    }

    fn cost_so_far(&self) -> f64 {
        self.base_cost + self.turn_cost
    }

    /// The shared text/usage rate limit: at most one per [`ACTIVITY_MIN_INTERVAL`].
    fn slow_lane_open(&mut self, now: std::time::Instant) -> bool {
        match self.last_slow {
            Some(prev) if now.duration_since(prev) < ACTIVITY_MIN_INTERVAL => false,
            _ => {
                self.last_slow = Some(now);
                true
            }
        }
    }
}

/// Truncate to at most `cap` chars, marking the cut (the char-wise sibling of the relay's
/// byte-wise `cap_line`).
fn cap_chars(s: &str, cap: usize) -> String {
    let mut capped: String = s.chars().take(cap).collect();
    if capped.len() < s.len() {
        capped.push('…');
    }
    capped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_contract::RANK_ACTIVITY_MARKER;

    fn detail(line: &str) -> serde_json::Value {
        let json = line
            .strip_prefix(RANK_ACTIVITY_MARKER)
            .expect("marker prefix")
            .trim();
        serde_json::from_str(json).expect("activity json")
    }

    /// An agent that starts, prints a diagnostic and dies leaves that diagnostic in the log.
    #[test]
    fn agent_stderr_earns_a_line() {
        let mut feed = ActivityFeed::new(RANK_ACTIVITY_MARKER, true);
        let now = std::time::Instant::now();
        let ev = AgentEvent::Raw {
            text: "credential file not found".to_string(),
            stream: RawStream::Stderr,
        };
        let line = feed.line_for("m", &ev, now).expect("stderr emits");
        let v = detail(&line);
        assert_eq!(v["kind"], "stderr");
        assert_eq!(v["detail"], "credential file not found");
        // Not rate limited: a dying agent's last words must not lose a race with a text delta.
        assert!(feed.line_for("m", &ev, now).is_some());
    }

    #[test]
    fn blank_stderr_and_agent_stdout_stay_quiet() {
        let mut feed = ActivityFeed::new(RANK_ACTIVITY_MARKER, true);
        let now = std::time::Instant::now();
        let blank = AgentEvent::Raw {
            text: "   ".to_string(),
            stream: RawStream::Stderr,
        };
        assert!(feed.line_for("m", &blank, now).is_none());
        let stdout = AgentEvent::Raw {
            text: "the whole transcript".to_string(),
            stream: RawStream::Stdout,
        };
        assert!(
            feed.line_for("m", &stdout, now).is_none(),
            "stdout is the verdict channel, never echoed back into the feed"
        );
    }

    #[test]
    fn a_disabled_feed_is_silent() {
        let mut feed = ActivityFeed::new(RANK_ACTIVITY_MARKER, false);
        let ev = AgentEvent::Log {
            level: "stage".to_string(),
            label: "openshell".to_string(),
            value: Some("creating sandbox".to_string()),
        };
        assert!(feed.line_for("m", &ev, std::time::Instant::now()).is_none());
    }
}
