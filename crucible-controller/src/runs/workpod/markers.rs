use std::time::Duration;

/// The label key every controller-owned work pod carries (`crucible.io/work-kind`), value the
/// [`WorkKind::label_value`] — the selector a sweep reconciles `work_pods` rows against, and the
/// same key `crucible deploy render-turn` stamps on a turn pod. A loop-run pod (`deploy render`,
/// which knows nothing of work kinds) gets it stamped on by [`stamp_run_pod`].
pub(crate) const WORK_KIND_LABEL: &str = "crucible.io/work-kind";

/// The single-line marker `crucible rank-grounded --marker` prints last, carrying the verdict JSON so
/// it survives the podman/git noise ahead of it in the turn pod's logs. The shared literal both the
/// engine emitter and this scraper match lives in `crucible-contract`.
pub use crucible_contract::VERDICT_MARKER;

/// The single-line marker `crucible scope --propose --json --marker` prints last, carrying the
/// ScopeReport JSON so it survives in pod logs. Analogous to [`VERDICT_MARKER`] for grounded ranking.
pub use crucible_contract::SCOPE_REPORT_MARKER;

/// The interim per-round marker the same `--marker` scope turn prints at each refine-round
/// boundary, carrying a small progress JSON. The live turn relay ([`crate::runs::turn_live`]) parses it
/// into a typed SSE event; nothing here persists it. Shared literal in `crucible-contract`.
pub use crucible_contract::SCOPE_PROGRESS_MARKER;

/// The within-round activity marker the same `--marker` scope turn prints while an agent turn
/// streams (tool calls, text snippets, usage samples, openshell stage banners), so the live view
/// isn't silent for the 10-20 minutes inside a round. Relay-only, never persisted. Shared literal
/// in `crucible-contract`.
pub use crucible_contract::SCOPE_ACTIVITY_MARKER;

/// The transcript marker a scope turn prints just before its report marker: base64 of the gzipped
/// session NDJSON. Shared literal in `crucible-contract`.
pub use crucible_contract::SCOPE_TRANSCRIPT_MARKER;

/// The fragment identifying the loop-run wrapper's session delimiter line
/// (`=== SESSION (rc=$rc) ===`, `crucible/src/deploy/render.rs`'s `wrapper_script`): everything
/// after that line in the pod's logs is the `cat` of the run's `state/session.jsonl`. Shared
/// literal in `crucible-contract`.
pub use crucible_contract::RUN_SESSION_DELIMITER;

/// The pack marker a SURVIVING scope turn prints before its report marker: base64 of the gzip'd
/// tar of the frozen pack dir (or an `{"error":…}` payload when the pack couldn't be emitted
/// whole). The pod executor's only way to land the pack at `pack_out` — the turn pod's filesystem
/// dies with it. Shared literal in `crucible-contract`.
pub use crucible_contract::SCOPE_PACK_MARKER;

/// How long a grounded-rank turn pod may run before the out-of-band timeout sweep
/// ([`sweep_timed_out_turns`]) kills it and fails its row. Grounding is a bounded grep-and-classify
/// turn, but the pod pays ~15 minutes of sandbox-image pull into its ephemeral podman storage
/// before the agent starts (measured live 2026-07-05), so the budget covers pull + turn. Flat: a
/// grounded turn has no gaming-refine knob to scale against.
pub(crate) const GROUNDED_RANK_TIMEOUT: Duration = Duration::from_secs(40 * 60);

/// The BASE of a scope turn's deadline: propose + validation (check + selftest) + basic non-gaming
/// refines, plus the same ~15-minute sandbox pull tax every turn pays. A `--gaming-refine-rounds 0`
/// (skip-review) turn gets exactly this. Real scope turns run LONGER when gaming review is enabled:
/// each concern→refine→re-review cycle is another full agent turn (3 cycles measured at ~82 minutes
/// live 2026-07-06), so the effective deadline scales — see [`scope_deadline`]. Named `_TIMEOUT` for
/// symmetry with the grounded constant, but it is a base, not the whole budget.
pub(crate) const SCOPE_TIMEOUT: Duration = Duration::from_secs(90 * 60);

/// How much each gaming-review cycle adds to a scope turn's deadline. A cycle is a full extra agent
/// turn (draft a fix for the reviewer's concern, then re-review it), so it needs turn-scale headroom
/// on top of the sandbox pull already amortized into the base. Sized so a 6-cycle allowance clears
/// 2h comfortably (90 + 6·30 = 270 min); see [`scope_deadline`].
const SCOPE_GAMING_CYCLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// A scope turn's effective deadline for the given gaming-refine allowance: the [`SCOPE_TIMEOUT`]
/// base plus [`SCOPE_GAMING_CYCLE_TIMEOUT`] per permitted cycle. Computed from the SAME
/// `scope_gaming_rounds` the turn's argv render forwards (`WorkPodSpec::scope`), so the sweep's
/// patience always matches what the pod was actually told it could do. Skip-review (`0`) = the base.
pub(crate) fn scope_deadline(gaming_rounds: u32) -> Duration {
    SCOPE_TIMEOUT + SCOPE_GAMING_CYCLE_TIMEOUT * gaming_rounds
}

/// A FAILED pod is kept this long after it went terminal so an operator can `kubectl logs` it, then a
/// sweep deletes it. Succeeded pods are deleted immediately on result collection (their verdict is
/// already in the ledger). Retention is also capped by count ([`sweep_failed_pod_overflow`],
/// `failed_pod_keep`): a failure storm sweeps everything past the newest N well before this window.
pub(crate) const FAILED_POD_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
