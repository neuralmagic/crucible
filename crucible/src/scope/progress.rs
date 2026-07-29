use crate::event::{AgentEvent, cost_of, estimate_cost};
use crate::refine::RoundKind;
use serde::Serialize;

/// The prefix of the interim progress marker `--marker` emits at each refine-round boundary, so a
/// live log tail can show where a 5-15 minute scope turn is instead of a black box. The controller's
/// live turn relay matches the same shared literal from `crucible-contract`.
pub use crucible_contract::SCOPE_PROGRESS_MARKER;

/// One interim progress beat, emitted just before a round's agent turn starts. Closed and small on
/// purpose, a live-view hint, never a result (the terminal [`ScopeReport`] is the result).
#[derive(Debug, Clone, Serialize)]
pub struct ScopeProgress {
    /// 1-based round number, same numbering as [`RoundRecord::round`].
    pub round: u32,
    pub kind: RoundKind,
    /// What the round is about to do, one human-readable line.
    pub doing: String,
    /// Total turn cost (USD) accumulated before this round started.
    pub cost_so_far: f64,
}

impl ScopeProgress {
    /// The full single-line marker: `CRUCIBLE_SCOPE_PROGRESS: {json}`.
    pub fn marker_line(&self) -> String {
        let json = serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string());
        format!("{SCOPE_PROGRESS_MARKER} {json}")
    }
}

/// Cap on the `doing` field: failure evidence can be pages of selftest output, and a progress beat
/// is a hint, not the trail (`SCOPE.md`/`REJECTED.md` carry the full evidence).
pub(super) const PROGRESS_DOING_CAP: usize = 240;

/// Truncate a `doing` line to [`PROGRESS_DOING_CAP`] chars, marking the cut.
pub(super) fn cap_doing(doing: &str) -> String {
    let mut capped: String = doing.chars().take(PROGRESS_DOING_CAP).collect();
    if capped.len() < doing.len() {
        capped.push('…');
    }
    capped
}

/// Print one progress marker line (stdout, so it rides the same pod-log channel as the report
/// marker). No-op unless `--marker` asked for machine-readable output.
pub(super) fn emit_progress(
    enabled: bool,
    round: u32,
    kind: RoundKind,
    doing: &str,
    cost_so_far: f64,
) {
    if !enabled {
        return;
    }
    let beat = ScopeProgress {
        round,
        kind,
        doing: cap_doing(doing),
        cost_so_far,
    };
    println!("{}", beat.marker_line());
}

/// The prefix of the within-round activity marker `--marker` emits while an agent turn streams:
/// `CRUCIBLE_SCOPE_ACTIVITY: {json}`, one line per observed tool call / text snippet / usage
/// sample. Progress beats mark round boundaries; these fill the 10-20 minutes inside a round so a
/// live log tail is never silent. The shared literal lives in `crucible-contract`.
pub use crucible_contract::SCOPE_ACTIVITY_MARKER;

/// One activity line's payload. Like [`ScopeProgress`]: a live-view hint, never a result.
#[derive(Debug, Clone, Serialize)]
pub struct ScopeActivity {
    /// `tool` | `text` | `usage` | `stage` | `truncated`.
    pub kind: &'static str,
    /// The tool name, `tool` lines only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A capped human-readable summary (tool input, text snippet, token count, stage banner).
    pub detail: String,
    /// Total scope cost (USD) observed so far: rounds already finished + the live turn's samples.
    pub cost_so_far: f64,
}

/// Caps on one activity line's fields, a hint for a ticker, never a transcript.
pub(super) const ACTIVITY_TOOL_CAP: usize = 80;
pub(super) const ACTIVITY_TEXT_CAP: usize = 120;

/// At most one non-tool activity line (text/usage) per this interval. A human-watchable ticker
/// needs ~1 update per few seconds; tool calls are API-round-trip paced already and emit freely.
pub(super) const ACTIVITY_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Total activity bytes one scope run may emit. The kubelet's 10 MiB log budget is shared with
/// the transcript (8 MiB pre-gzip cap), the pack (4 MiB pre-gzip cap), and the report marker;
/// 256 KiB of activity is noise against that headroom while still covering hours of ticking
/// (~200 bytes/line × one line per 5s ≈ 144 KiB over an hour).
const ACTIVITY_BYTE_BUDGET: usize = 256 * 1024;

/// The bounded within-round activity feed: turns the agent events the scope sinks otherwise
/// swallow into `CRUCIBLE_SCOPE_ACTIVITY:` stdout lines. Three bounds, in order: the `--marker`
/// gate (disabled = fully silent), the per-line char caps + text/usage rate limit, and the total
/// byte budget (exhausted = one honest `truncated` line, then silence).
pub struct ActivityFeed {
    enabled: bool,
    pub(super) bytes_left: usize,
    truncated: bool,
    last_slow: Option<std::time::Instant>,
    /// Cost accumulated by rounds that already finished.
    base_cost: f64,
    /// The live turn's latest cumulative cost sample (authoritative if reported, else estimated).
    turn_cost: f64,
}

impl ActivityFeed {
    pub fn new(enabled: bool) -> Self {
        ActivityFeed {
            enabled,
            bytes_left: ACTIVITY_BYTE_BUDGET,
            truncated: false,
            last_slow: None,
            base_cost: 0.0,
            turn_cost: 0.0,
        }
    }

    /// Rebase at a turn boundary: prior rounds' total becomes the floor the live samples add to.
    pub(super) fn begin_turn(&mut self, cost_so_far: f64) {
        self.base_cost = cost_so_far;
        self.turn_cost = 0.0;
    }

    /// Observe one streamed event; print its activity line if it earns one.
    pub(super) fn observe(&mut self, model: &str, ev: &AgentEvent) {
        if let Some(line) = self.line_for(model, ev, std::time::Instant::now()) {
            println!("{line}");
        }
    }

    /// The marker line `ev` earns at `now`, if any. Split from [`Self::observe`] so tests drive
    /// the rate limit and budget with controlled instants instead of capturing stdout.
    pub(super) fn line_for(
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
            AgentEvent::Tool { name, summary, .. } => ScopeActivity {
                kind: "tool",
                name: Some(name.clone()),
                detail: cap_chars(summary, ACTIVITY_TOOL_CAP),
                cost_so_far: self.cost_so_far(),
            },
            AgentEvent::Text { delta } if !delta.trim().is_empty() => {
                if !self.slow_lane_open(now) {
                    return None;
                }
                ScopeActivity {
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
                ScopeActivity {
                    kind: "usage",
                    name: None,
                    detail: format!("{} tokens", t.total),
                    cost_so_far: self.cost_so_far(),
                }
            }
            // The openshell driver's orchestration banners (sandbox create / image pull / uploads).
            AgentEvent::Log { level, value, .. } if level == "stage" => ScopeActivity {
                kind: "stage",
                name: None,
                detail: cap_chars(value.as_deref().unwrap_or_default(), ACTIVITY_TEXT_CAP),
                cost_so_far: self.cost_so_far(),
            },
            _ => return None,
        };
        let line = format!(
            "{SCOPE_ACTIVITY_MARKER} {}",
            serde_json::to_string(&activity).unwrap_or_else(|_| "{}".to_string())
        );
        if line.len() + 1 > self.bytes_left {
            self.truncated = true;
            let tail = ScopeActivity {
                kind: "truncated",
                name: None,
                detail: format!(
                    "activity feed truncated ({ACTIVITY_BYTE_BUDGET} byte budget spent)"
                ),
                cost_so_far: self.cost_so_far(),
            };
            return Some(format!(
                "{SCOPE_ACTIVITY_MARKER} {}",
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
