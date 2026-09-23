//! Types every slice shares: the issue lifecycle [`Status`], who parked it and why, the launch
//! [`Trigger`] and its origin, sort direction, ledger rows and the key helpers.

use crate::wire_enum::wire_enum;

// Kebab-case, matching the status vocabulary exactly.
wire_enum!(Status, "issue status", both, {
    Status::New => "new",
    Status::Scoped => "scoped",
    Status::AwaitingApproval => "awaiting-approval",
    Status::Building => "building",
    Status::Running => "running",
    Status::PrOpen => "pr-open",
    Status::Parked => "parked",
    Status::Done => "done",
});

// The DB CHECK constraint enforces exactly these two spellings.
wire_enum!(ParkedBy, "parked_by", both, {
    ParkedBy::Machine => "machine",
    ParkedBy::Human => "human",
});

/// One day's summed ledger cost (`ledger_day_total` grouped by day, newest first).
#[derive(Debug, Clone)]
pub struct LedgerDay {
    pub(crate) day: String,
    pub(crate) total_usd: f64,
}

/// One cost tag's summed ledger cost within a day (`ledger_day_by_tag`), biggest spender first.
#[derive(Debug, Clone)]
pub struct LedgerTagTotal {
    pub(crate) tag: String,
    pub(crate) total_usd: f64,
}

/// Ascending or descending — the `dir=` query param paired with [`SortKey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, strum::EnumIter)]
pub enum SortDir {
    Asc,
    #[default]
    Desc,
}

wire_enum!(SortDir, "sort direction", parse_only, {
    SortDir::Asc => "asc",
    SortDir::Desc => "desc",
});

/// A filesystem- and slug-safe name for an issue key (`owner/repo#7` → `owner_repo_7`) — the
/// `pack_tarballs`/`pack_steering` key and the pack PR branch token.
pub(crate) fn sanitize_key(key: &str) -> String {
    key.replace(['/', '#', ':'], "_")
}

/// Cap on an error chain recorded as event evidence: enough to carry the render's real cause (a
/// TOML parse/render line) without dumping a page into the NDJSON event log.
const EVENT_EVIDENCE_CAP: usize = 2000;

/// Truncate an `{e:#}` error chain to [`EVENT_EVIDENCE_CAP`] chars, marking the cut.
pub(crate) fn truncate_chain(chain: &str) -> String {
    let mut capped: String = chain.chars().take(EVENT_EVIDENCE_CAP).collect();
    if capped.len() < chain.len() {
        capped.push('…');
    }
    capped
}

/// The issue lifecycle. Stored as the TEXT `issues.status`; `new` is the only entry state, `done`
/// the only terminal one — `parked` is a resting state a machine park can revive and a human
/// sweep can bump, so it stays in the reconcile set (`non_terminal_keys` keys off `<> 'done'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Status {
    New,
    Scoped,
    AwaitingApproval,
    /// An approved pack whose image builds are in flight: the run is blocked until every
    /// `[build.<name>]` the pack declares has a pinned digest. Success launches the run
    /// (`building` → `running`); a failed/timed-out build parks it with the build-log pointer.
    /// Between `awaiting-approval` and `running`, and only entered by a pack that declares builds
    /// — a pack with none launches directly, exactly as before.
    Building,
    Running,
    PrOpen,
    Parked,
    Done,
}

/// Who parked an issue. A `machine` park (no repro, can't isolate) auto-unparks when the issue
/// content materially changes; a `human` park is sticky. The DB CHECK constraint
/// enforces exactly these two spellings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum ParkedBy {
    Machine,
    Human,
}

/// Why an issue is machine-parked. `park()` takes this (not a bare `&str`) so a call site can
/// never park with a "reason" logic downstream can't recognize — the rendering that lands in
/// `issues.parked_reason` (and the event-log line the park appends) happens exactly once, in
/// `Display`. `parse` is its inverse for the handful of call sites that need to know WHICH reason
/// an already-persisted or event-log-replayed row carries — it never fails: a row this enum
/// doesn't recognize (predates this type, or a wording this pass didn't enumerate) reads back as
/// `Legacy`, verbatim, so an old row is never mistaken for a new variant and never blocks a read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParkReason {
    /// The upstream issue closed. The `upstream=` filter's exact-match bind and the reopen edge's
    /// revival check both key off this variant (equality, not prefix).
    UpstreamClosed,
    /// Beyond the configured rank horizon with no upstream activity. The ONLY auto-unparkable
    /// machine park — `reconcile_parked`'s stale auto-unpark matches this variant, not a prefix.
    StaleRankHorizon { days: u32 },
    /// The grounded ranker found the ask already implemented in the current checkout. Actionable:
    /// a human should close the upstream issue. The SPA's "closable upstream" badge keys off this
    /// variant via a server-computed field (`IssueDto::stale_closable`) instead of re-matching text.
    StaleAlreadyImplemented { rationale: String },
    /// The grounded ranker demoted the issue to tier N.
    DemotedToN { rationale: String },
    /// The text-only ranker (no grounded confirmation reached) said tier N: unscopeable.
    Unscopeable,
    /// The ranker judged the issue off-affinity for the performance loop (docs, CI, chores,
    /// general feature work): parked before any grounded escalation or scope turn spends on it.
    Unrelated { rationale: String },
    /// The scope's declared build set exceeds `build_pod_cap` and can never be admitted together.
    BuildCapUnadmittable { declared: usize, cap: u32 },
    /// A declared image build failed or timed out. `evidence` is the build-log pointer when the
    /// backend published one. The admin rebuild endpoint matches this variant (not a prefix) to
    /// decide whether a forced rebuild should unpark back to `building`.
    ImageBuildFailed { evidence: Option<String> },
    /// The scope pipeline died at a named stage before freezing a pack. `detail` is the engine's
    /// own free text (`crucible scope --json`'s stage detail) — opaque past `stage`, never further
    /// parsed; the engine crate that produces it is out of this pass's scope.
    ScopeFailed { stage: String, detail: String },
    /// A dead scope proposal with no failing stage on record at all.
    ScopeProducedNoPack,
    /// The run's loop pod finished but its logs carry no SESSION delimiter at all.
    NoSessionDelimiter { run_id: String, tail: String },
    /// The run's loop pod reached the SESSION delimiter but published no session — it crashed
    /// before writing a single event. `rc` is the wrapper's exit code.
    NoSessionEmpty {
        run_id: String,
        rc: Option<i32>,
        tail: String,
    },
    /// The API definitively reports that a dispatched pod is gone, but no terminal session was
    /// published. This is infrastructure loss, never successful workload completion.
    RunPodLost {
        run_id: String,
        pod: String,
        last_observation: Option<String>,
    },
    /// Session bytes were present, but did not contain a valid terminal shutdown event.
    TerminalSessionInvalid { run_id: String, detail: String },
    /// The run's own shutdown reported `outcome: "error"`; `engine_reason` rides verbatim (opaque
    /// free text from the loop). The shutdown reason often names only where the run stopped, so
    /// `cause` carries the failing task's note — the diagnosis, which outlives the deleted pod.
    RunErrored {
        engine_reason: Option<String>,
        cause: Option<String>,
    },
    /// A local-mode playbook run's engine subprocess died without publishing a session, so there
    /// is no outcome to ingest. `detail` is the supervisor's own account of how it ended.
    LocalRunFailed { run_id: String, detail: String },
    /// A launch's dispatch image (or local engine binary) carries a contract version other than
    /// this controller's. Deterministic: the same launch cannot succeed until the pin changes, so
    /// the startup contract check is the only automatic unpark.
    ContractRejected {
        image: String,
        engine_version: String,
        controller_version: String,
    },
    /// A launch needs a render option the linked engine does not define. Deterministic, like a
    /// version mismatch, but a matching engine version is no cure: the pin has to move forward.
    UnsupportedTurnOption { option: String },
    /// The pack's sandbox image failed the capability preflight at dispatch. Deterministic until
    /// the pack, its image, or the dispatch default changes.
    ImagePreflightRefused { image: String, detail: String },
    /// A playbook launch's stored row is gone, so there is nothing to dispatch. Never expected —
    /// the row is written in the same transaction as the issue — but a launch must not run on
    /// guessed values, so the row parks instead.
    PlaybookLaunchMissing,
    /// The launch's scope binds secrets the launcher's principals do not cover, declares a name
    /// nothing is bound to, or was bound against a pack revision a pin bump moved.
    SecretsUnresolved { detail: String },
    /// A standing launch (a schedule, a one-shot, a watch) fired with no usable snapshot of its
    /// owner's groups, so there is no authorization to launch a scope's bound secrets under.
    OwnerStale {
        trigger: crate::model::Trigger,
        id: String,
        detail: String,
    },
    /// A stored or event-log-replayed reason this build doesn't recognize: a pre-enum row, or a
    /// park call site this pass didn't enumerate. `parse` NEVER produces an `Err` — this is the
    /// catch-all it falls back to instead.
    Legacy(String),
}

impl std::fmt::Display for ParkReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UpstreamClosed => write!(f, "upstream closed"),
            Self::StaleRankHorizon { days } => {
                write!(f, "stale: no upstream activity in {days} days")
            }
            Self::StaleAlreadyImplemented { rationale: _ } => {
                // Rationale deliberately NOT interpolated: it rides a sibling "new -> new" event,
                // exactly as today — keeps this variant's rendering a stable equality target.
                write!(f, "stale per grounded ranker: already implemented")
            }
            Self::DemotedToN { rationale } => {
                write!(f, "demoted to N per grounded ranker: {rationale}")
            }
            Self::Unscopeable => write!(f, "unscopeable per ranker"),
            Self::Unrelated { rationale } => {
                write!(
                    f,
                    "unrelated to the performance loop per ranker: {rationale}"
                )
            }
            Self::BuildCapUnadmittable { declared, cap } => write!(
                f,
                "pack declares {declared} image build(s) but build_pod_cap={cap}: the set can \
                 never be admitted together — raise CONTROLLER_BUILD_POD_CAP or split the builds"
            ),
            Self::ImageBuildFailed { evidence } => write!(
                f,
                "image build failed: {}",
                evidence
                    .as_deref()
                    .unwrap_or("no build-log pointer published")
            ),
            Self::ScopeFailed { stage, detail } => write!(f, "{stage}: {detail}"),
            Self::ScopeProducedNoPack => write!(f, "scope produced no frozen pack"),
            Self::NoSessionDelimiter { run_id, tail } => write!(
                f,
                "run {run_id} finished but its pod logs carry no SESSION delimiter at all (the \
                 wrapper died before the dump, or kubelet rotation truncated the logs). Last pod \
                 log lines:\n{tail}"
            ),
            Self::NoSessionEmpty { run_id, rc, tail } => write!(
                f,
                "run {run_id} finished but published no session log: the wrapper reached the \
                 SESSION delimiter (rc={}) with an empty session.jsonl, so the loop failed before \
                 writing any event. Last pod log lines before the delimiter:\n{tail}",
                rc.map(|c| c.to_string()).unwrap_or_else(|| "?".to_string())
            ),
            Self::RunPodLost {
                run_id,
                pod,
                last_observation,
            } => write!(
                f,
                "run {run_id} infrastructure failure: pod {pod} disappeared before terminal session evidence{}; redispatch the run",
                last_observation
                    .as_deref()
                    .map(|detail| format!(" (last observed: {detail})"))
                    .unwrap_or_default()
            ),
            Self::TerminalSessionInvalid { run_id, detail } => {
                write!(
                    f,
                    "run {run_id} has no valid terminal session evidence: {detail}; redispatch the run"
                )
            }
            Self::RunErrored {
                engine_reason,
                cause,
            } => {
                let reason = engine_reason.as_deref().unwrap_or("no reason published");
                match cause {
                    Some(cause) => write!(f, "run errored: {reason}: {cause}"),
                    None => write!(f, "run errored: {reason}"),
                }
            }
            Self::SecretsUnresolved { detail } => write!(f, "secrets: {detail}"),
            Self::OwnerStale {
                trigger,
                id,
                detail,
            } => write!(
                f,
                "{} {id} owner snapshot is stale: {detail}",
                trigger.as_str()
            ),
            Self::LocalRunFailed { run_id, detail } => write!(
                f,
                "run {run_id} ran locally and published no session: {detail}"
            ),
            Self::PlaybookLaunchMissing => {
                write!(f, "playbook launch row is missing; nothing to run")
            }
            Self::UnsupportedTurnOption { option } => {
                write!(f, "unsupported turn option: {option}")
            }
            Self::ImagePreflightRefused { image, detail } => {
                write!(f, "image preflight refused {image}: {detail}")
            }
            Self::ContractRejected {
                image,
                engine_version,
                controller_version,
            } => write!(
                f,
                "contract rejection: {image} carries engine contract {engine_version}, this \
                 controller is built against {controller_version}"
            ),
            Self::Legacy(s) => write!(f, "{s}"),
        }
    }
}

impl ParkReason {
    /// Parse a persisted or event-log-replayed `parked_reason` back into its variant. Infallible —
    /// anything unrecognized is `Legacy`, verbatim. Prefix checks here are the SAME prefixes the
    /// old call sites string-matched; the difference is there is now exactly one place they live.
    pub(crate) fn parse(s: &str) -> Self {
        if s == "upstream closed" {
            return Self::UpstreamClosed;
        }
        if s == "stale per grounded ranker: already implemented" {
            return Self::StaleAlreadyImplemented {
                rationale: String::new(),
            };
        }
        if s == "unscopeable per ranker" {
            return Self::Unscopeable;
        }
        if s == "scope produced no frozen pack" {
            return Self::ScopeProducedNoPack;
        }
        if s == "playbook launch row is missing; nothing to run" {
            return Self::PlaybookLaunchMissing;
        }
        if let Some(rest) = s.strip_prefix("stale: no upstream activity in ")
            && let Some(days) = rest.strip_suffix(" days").and_then(|d| d.parse().ok())
        {
            return Self::StaleRankHorizon { days };
        }
        if let Some(detail) = s.strip_prefix("secrets: ") {
            return Self::SecretsUnresolved {
                detail: detail.to_string(),
            };
        }
        if let Some(option) = s.strip_prefix("unsupported turn option: ") {
            return Self::UnsupportedTurnOption {
                option: option.to_string(),
            };
        }
        if let Some(rest) = s.strip_prefix("image preflight refused ")
            && let Some((image, detail)) = rest.split_once(": ")
        {
            return Self::ImagePreflightRefused {
                image: image.to_string(),
                detail: detail.to_string(),
            };
        }
        if let Some(rest) = s.strip_prefix("contract rejection: ")
            && let Some((image, rest)) = rest.split_once(" carries engine contract ")
            && let Some((engine_version, controller_version)) =
                rest.split_once(", this controller is built against ")
        {
            return Self::ContractRejected {
                image: image.to_string(),
                engine_version: engine_version.to_string(),
                controller_version: controller_version.to_string(),
            };
        }
        if let Some(evidence) = s.strip_prefix("image build failed: ") {
            let evidence =
                (evidence != "no build-log pointer published").then(|| evidence.to_string());
            return Self::ImageBuildFailed { evidence };
        }
        // NoSessionDelimiter / NoSessionEmpty / RunPodLost / TerminalSessionInvalid / RunErrored / DemotedToN / Unrelated /
        // BuildCapUnadmittable / ScopeFailed carry enough free text (log tails, rationale, arbitrary stage names) that a
        // round-trip parse isn't attempted — nothing downstream needs to recover THEIR structure
        // from a stored string today (see inventory: none of them are string-matched). They render
        // via Display when produced fresh; a persisted/replayed row of theirs reads back as
        // `Legacy` (verbatim text, so display is unaffected) until/unless a real consumer needs it.
        Self::Legacy(s.to_string())
    }

    /// Gates `reconcile_parked`'s ONLY auto-unpark path. Replaces the
    /// `starts_with("stale: no upstream activity")` match.
    #[cfg(feature = "autoresearch")]
    pub(crate) fn auto_unparkable_on_activity(&self) -> bool {
        matches!(self, Self::StaleRankHorizon { .. })
    }

    /// The SPA's secrets call-out: the refusal a launch was parked on, which names the declared
    /// name nothing is bound to or the secret the launcher does not own. Derived here so no page
    /// has to match on the reason's wording.
    pub(crate) fn secrets_refusal(&self) -> Option<&str> {
        match self {
            Self::SecretsUnresolved { detail } => Some(detail),
            _ => None,
        }
    }

    /// The SPA's "closable upstream" signal (`IssueDto::stale_closable`).
    #[cfg(feature = "autoresearch")]
    pub(crate) fn is_stale_closable(&self) -> bool {
        matches!(self, Self::StaleAlreadyImplemented { .. })
    }

    /// Replaces `api/builds.rs`'s `starts_with("image build failed:")`.
    #[cfg(feature = "autoresearch")]
    pub(crate) fn is_image_build_failure(&self) -> bool {
        matches!(self, Self::ImageBuildFailed { .. })
    }
}

/// Where a launch came from. Downstream a launch is a launch — same row, same dispatch — so this
/// exists to keep the one-shot surface from listing a schedule's firings, and to say in the audit
/// trail which surface authorized a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, strum::EnumIter)]
pub(crate) enum LaunchOrigin {
    /// A form POST.
    #[default]
    Manual,
    /// A deferred one-shot whose `fire_at` came due.
    Deferred,
    /// A recurring schedule's sweep.
    Schedule,
    /// A draft pack test-fired from the authoring studio.
    Draft,
    /// A tracker watch's sweep: one launch per item its query matched.
    Watch,
}

crate::wire_enum::wire_enum!(LaunchOrigin, "launch origin", both, {
    LaunchOrigin::Manual => "manual",
    LaunchOrigin::Deferred => "deferred",
    LaunchOrigin::Schedule => "schedule",
    LaunchOrigin::Draft => "draft",
    LaunchOrigin::Watch => "watch",
});

/// Which sidecar a core row belongs to. The DB discriminator; a sidecar module knows its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Trigger {
    Schedule,
    Deferred,
    Watch,
}

crate::wire_enum::wire_enum!(Trigger, "trigger", both, {
    Trigger::Schedule => "schedule",
    Trigger::Deferred => "deferred",
    Trigger::Watch => "watch",
});

impl Trigger {
    /// The origin the launches this trigger mints carry.
    pub(crate) fn origin(self) -> LaunchOrigin {
        match self {
            Trigger::Schedule => LaunchOrigin::Schedule,
            Trigger::Deferred => LaunchOrigin::Deferred,
            Trigger::Watch => LaunchOrigin::Watch,
        }
    }

    /// The event-log key a trigger's own transitions (enabled, disabled, edited) are recorded
    /// under. Its firings are recorded under the launch key they minted, like every other launch.
    pub(crate) fn event_key(self, id: &str) -> String {
        format!("{}:{id}", self.as_str())
    }
}

/// The wall-clock ceiling a launcher gives one playbook run, in the engine's `--max-time` grammar
/// (`90s`, `30m`, `2h`, a bare number = seconds). Parsed here because the string is rendered into
/// a pod's argv: an unparsed one would reach the engine as a flag value it silently drops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaxTime {
    raw: String,
    secs: u64,
}

impl MaxTime {
    /// Parse a duration string, mirroring the engine's grammar. `Err` carries the launcher-facing
    /// message.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        let split = trimmed
            .find(|c: char| c.is_ascii_alphabetic())
            .unwrap_or(trimmed.len());
        let (digits, unit) = trimmed.split_at(split);
        let bad = || format!("max_time {raw:?} is not a duration (try `90s`, `30m`, `2h`)");
        let n: u64 = digits.parse().map_err(|_| bad())?;
        let per_unit = match unit {
            "" | "s" | "sec" => 1,
            "m" | "min" => 60,
            "h" | "hr" => 3600,
            _ => return Err(bad()),
        };
        let secs = n.checked_mul(per_unit).ok_or_else(bad)?;
        if secs == 0 {
            return Err("max_time must be greater than zero".to_string());
        }
        Ok(MaxTime {
            raw: trimmed.to_string(),
            secs,
        })
    }

    /// A whole-hours ceiling. Infallible, so a compile-time default needs no unwrap.
    pub fn hours(n: u64) -> Self {
        MaxTime {
            raw: format!("{n}h"),
            secs: n.saturating_mul(3600),
        }
    }

    /// The string as rendered into the pod argv.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The engine's own ceiling type, for a linked render.
    pub fn engine(&self) -> Result<crucible::duration::MaxTime, crucible::duration::BadMaxTime> {
        self.raw.parse()
    }

    pub fn secs(&self) -> u64 {
        self.secs
    }
}

impl std::fmt::Display for MaxTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

/// `value` with every object's keys in sorted order, recursively, so a rendering or a digest of
/// it does not depend on insertion order.
pub(crate) fn sorted_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> =
                map.into_iter().map(|(k, v)| (k, sorted_json(v))).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            serde_json::Value::Object(entries.into_iter().collect())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sorted_json).collect())
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_time_takes_the_engine_grammar_and_refuses_everything_else() {
        for (raw, secs) in [("90s", 90), ("30m", 1800), ("2h", 7200), ("45", 45)] {
            let parsed = MaxTime::parse(raw).expect(raw);
            assert_eq!(parsed.secs(), secs);
            assert_eq!(parsed.as_str(), raw);
        }
        assert_eq!(MaxTime::parse(" 30m ").expect("trimmed").as_str(), "30m");
        assert_eq!(MaxTime::hours(4).secs(), 14_400);
        for bad in ["", "garbage", "10x", "-5", "1.5h", "30 m", "h", "0", "0s"] {
            assert!(MaxTime::parse(bad).is_err(), "{bad:?} is not a duration");
        }
    }

    #[test]
    fn sorted_json_orders_keys_at_every_depth() {
        let value = serde_json::json!({"b": {"y": 1, "x": [ {"k": 2, "a": 3} ]}, "a": 0});
        assert_eq!(
            sorted_json(value).to_string(),
            r#"{"a":0,"b":{"x":[{"a":3,"k":2}],"y":1}}"#
        );
    }
}
