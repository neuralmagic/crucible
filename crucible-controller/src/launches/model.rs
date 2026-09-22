//! The launches slice's launch, run and cursor types.

use crate::model::Status;

use crate::model::LaunchOrigin;

use crate::wire_enum::wire_enum;

use anyhow::Result;

/// What one launch of a registered playbook is authorized to run with, as written at adopt time.
/// `repo` and `title` are the registry row's, so a launch groups and reads sanely everywhere the
/// SPA keys on them; the pack itself is the launch's own copy of the registered tarball.
#[derive(Debug, Clone)]
pub(crate) struct NewPlaybookLaunch<'a> {
    pub playbook: &'a str,
    pub repo: &'a str,
    pub title: &'a str,
    /// The validated `{name: value}` object.
    pub params: &'a serde_json::Value,
    pub schema_digest: &'a str,
    pub max_cost: f64,
    pub max_time: &'a crate::model::MaxTime,
    /// Whether this run may move the schedule dedupe state (the cursor, the seen-set) forward.
    pub advance_dedupe: bool,
    /// The schedule whose cursor this run may advance. `None` moves nothing, which is what makes
    /// an ad-hoc launch inert by default.
    pub dedupe_schedule: Option<&'a str>,
    pub origin: LaunchOrigin,
    /// The draft version this run froze. `None` = `playbook` names a registered pack.
    pub draft_version: Option<i64>,
    pub created_by: Option<&'a str>,
    /// The groups the launcher's claims carried, as a JSON array. `created_by` is only half an
    /// identity: a group-owned secret needs the memberships too.
    pub launcher_groups: Option<&'a serde_json::Value>,
}

/// A deferred one-shot's lifecycle. Every state but `Pending` is terminal: a one-shot fires once
/// and completes, which is the whole difference between it and a schedule. `Failed` is a firing
/// the sweep claimed and could not launch, parked there so it stops being re-claimed ahead of the
/// one-shots behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub(crate) enum OneShotStatus {
    Pending,
    Fired,
    Canceled,
    Failed,
}

/// One `playbook_launches` row, read back by key at dispatch because a dequeued key carries no
/// payload.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlaybookLaunch {
    pub playbook: String,
    /// The values, sorted by name so a launch always renders the same argv.
    pub params: Vec<(String, String)>,
    pub schema_digest: String,
    pub max_cost: f64,
    pub max_time: crate::model::MaxTime,
    /// Who launched it and the groups their claims carried, as the launch was authorized. A
    /// dispatch happens long after that session is gone, so the ownership check at launch reads
    /// these rather than a live claim.
    pub created_by: Option<String>,
    pub launcher_groups: Vec<String>,
    /// The draft version this launch froze, if any. A draft test-fire resolves no bindings.
    pub draft_version: Option<i64>,
    /// The exposure recorded on the launch row itself, which a draft one-shot recomputes from the
    /// exact content it launches. `None` on a registered launch, which reads the registry row.
    pub exposure: Option<crate::playbooks::exposure::Exposure>,
}

/// Where a schedule's cursor reads its next value: a dotted path into the run result, whose first
/// segment is the task whose `output` carries the value (`$.scan.newest_created_at`). Only that
/// closed grammar — a pack declares what it emits, and a general JSONPath would let a schedule
/// address structure no task promised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResultPath {
    raw: String,
    keys: Vec<String>,
}

impl ResultPath {
    /// Parse `$.a.b`. Rejects anything else so a bad path is a field-level refusal at write time
    /// rather than a cursor that silently never resolves.
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let Some(body) = raw.strip_prefix("$.") else {
            return Err(format!("{raw:?} must start with `$.`"));
        };
        let keys: Vec<String> = body.split('.').map(str::to_string).collect();
        if keys.iter().any(|k| {
            k.is_empty()
                || !k
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        }) {
            return Err(format!(
                "{raw:?} must be dotted keys of letters, digits, `_` or `-`"
            ));
        }
        Ok(ResultPath {
            raw: raw.to_string(),
            keys,
        })
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.raw
    }

    /// The scalar this path addresses, rendered the way a param value is. `None` when the document
    /// has no such field or the field is an array, an object, or null — a param is one string, and
    /// guessing a serialization for the rest would put the controller inside the pack's contract.
    pub(crate) fn extract(&self, doc: &serde_json::Value) -> Option<String> {
        let mut at = doc;
        for key in &self.keys {
            at = at.get(key)?;
        }
        match at {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }
}

/// The key of one file a run's task captured (`rollup/STATE.json`), as
/// `GET /api/runs/{id}/files/{key}` serves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunFileKey(String);

impl RunFileKey {
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if !crate::runs::run_files::is_key(raw) {
            return Err(format!(
                "{raw:?} must be a `$.task.field` result path or a `task/file` run-file key"
            ));
        }
        Ok(RunFileKey(raw.to_string()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where in the pack a file cursor is written before a firing runs: a relative path that cannot
/// leave the pack directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackPath(String);

impl PackPath {
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let safe = !raw.is_empty()
            && !raw.starts_with('/')
            && raw
                .split('/')
                .all(|c| !c.is_empty() && c != "." && c != ".." && !c.contains('\\'));
        if !safe {
            return Err(format!(
                "{raw:?} must be a relative path inside the pack, without `.` or `..` segments"
            ));
        }
        Ok(PackPath(raw.to_string()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn under(&self, pack_dir: &std::path::Path) -> std::path::PathBuf {
        pack_dir.join(&self.0)
    }
}

/// A schedule's cursor: what one firing's finished run leaves behind, and how the next firing
/// receives it. A field of the result rides as a param; a captured file is written into the pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CursorSpec {
    Field { from: ResultPath, param: String },
    File { from: RunFileKey, path: PackPath },
}

impl CursorSpec {
    /// Build one from what a caller wrote, with the messages a field-level 422 reports. The
    /// grammar of `from` decides the delivery: a `$.` path is passed as `param`, a run-file key
    /// is written to `path`, and naming the other one (or both) is a refusal.
    pub(crate) fn parse(
        from: &str,
        param: Option<&str>,
        path: Option<&str>,
    ) -> Result<Self, (&'static str, String)> {
        let param = param.map(str::trim).filter(|p| !p.is_empty());
        let path = path.map(str::trim).filter(|p| !p.is_empty());
        if from.trim().starts_with("$.") {
            let from = ResultPath::parse(from).map_err(|e| ("cursor.from", e))?;
            if path.is_some() {
                return Err((
                    "cursor.path",
                    "a result-path cursor is passed as a param, not written to a path".to_string(),
                ));
            }
            let Some(param) = param else {
                return Err((
                    "cursor.param",
                    "name the param the cursor value is passed as".to_string(),
                ));
            };
            return Ok(CursorSpec::Field {
                from,
                param: param.to_string(),
            });
        }
        let from = RunFileKey::parse(from).map_err(|e| ("cursor.from", e))?;
        if param.is_some() {
            return Err((
                "cursor.param",
                "a run-file cursor is written to a path, not passed as a param".to_string(),
            ));
        }
        let Some(path) = path else {
            return Err((
                "cursor.path",
                "name the pack path the cursor file is written to".to_string(),
            ));
        };
        let path = PackPath::parse(path).map_err(|e| ("cursor.path", e))?;
        Ok(CursorSpec::File { from, path })
    }

    /// Decode stored columns. Like [`InputKind::from_parts`] this never errors: a row whose path
    /// no longer parses reads as no cursor, so one bad row degrades dedupe instead of bailing the
    /// sweep that fires every other schedule.
    pub(crate) fn from_columns(
        from: Option<&str>,
        param: Option<&str>,
        path: Option<&str>,
    ) -> Option<Self> {
        CursorSpec::parse(from?, param, path).ok()
    }

    /// The `cursor_from` column: the result path or the run-file key as written.
    pub(crate) fn source(&self) -> &str {
        match self {
            CursorSpec::Field { from, .. } => from.as_str(),
            CursorSpec::File { from, .. } => from.as_str(),
        }
    }

    /// The param a field cursor is passed as; `None` for a file cursor.
    pub(crate) fn param(&self) -> Option<&str> {
        match self {
            CursorSpec::Field { param, .. } => Some(param),
            CursorSpec::File { .. } => None,
        }
    }

    /// The pack path a file cursor is written to; `None` for a field cursor.
    pub(crate) fn path(&self) -> Option<&str> {
        match self {
            CursorSpec::Field { .. } => None,
            CursorSpec::File { path, .. } => Some(path.as_str()),
        }
    }
}

/// One launch as the runs surface reads it: the frozen authorization a relaunch prefills from,
/// joined to what the run did (the issue's status, the spend its runs booked).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlaybookRun {
    pub key: String,
    pub playbook: String,
    /// The registry row's description, or `None` when the playbook has since been deregistered.
    pub description: Option<String>,
    /// The values exactly as stored, so a prefill re-renders what was launched.
    pub params: serde_json::Value,
    pub schema_digest: String,
    /// The registry's current digest, so a prefill can warn that the form moved under the launch.
    pub current_schema_digest: Option<String>,
    pub max_cost: f64,
    pub max_time: String,
    pub advance_dedupe: bool,
    pub origin: LaunchOrigin,
    /// The draft version this launch froze; `None` when `playbook` names a registered pack.
    pub draft_version: Option<i64>,
    /// The schedule this launch belongs to: the one that fired it, or the one whose cursor it was
    /// opted into advancing.
    pub schedule: Option<String>,
    pub status: Status,
    pub parked_reason: Option<String>,
    /// The inference provider the launch pinned on its issue row; `None` resolves the defaults.
    pub agent_provider: Option<String>,
    /// The model pinned alongside it; `None` takes the provider's default.
    pub agent_model: Option<String>,
    /// Summed `runs.cost_usd` for this launch's runs; `None` until one books spend.
    pub cost_usd: Option<f64>,
    pub runs: i64,
    /// Task attempts across this launch's runs that ended `transport`.
    pub transport_losses: i64,
    pub created_by: Option<String>,
    pub created_at: String,
}

crate::wire_enum::wire_enum!(OneShotStatus, "one-shot status", both, {
    OneShotStatus::Pending => "pending",
    OneShotStatus::Fired => "fired",
    OneShotStatus::Canceled => "canceled",
    OneShotStatus::Failed => "failed",
});

#[cfg(test)]
mod tests {
    use super::*;

    /// The cursor's path grammar is a closed subset, and everything outside it is refused at write
    /// time rather than stored as a cursor that never resolves.
    #[test]
    fn result_path_takes_dotted_keys_and_refuses_everything_else() {
        for raw in [
            "$.newest_created_at",
            "$.scan.newest_created_at",
            "$.scan.a-b_c.d0",
            "  $.scan.newest  ",
        ] {
            assert!(ResultPath::parse(raw).is_ok(), "{raw:?} is a path");
        }
        for raw in [
            "", "$", "$.", "newest", "$.a..b", "$.a.b.", "$['a']", "$.a b", "$.a/b", "$.*",
        ] {
            assert!(ResultPath::parse(raw).is_err(), "{raw:?} is not a path");
        }
    }

    /// Extraction yields one param value or nothing: a param is a string, and an array or object
    /// has no rendering the controller may choose on the pack's behalf.
    #[test]
    fn result_path_extracts_scalars_only() {
        let doc = serde_json::json!({
            "scan": {
                "newest_created_at": "2026-08-23T00:00:00Z",
                "count": 12,
                "empty": false,
                "items": ["a", "b"],
                "nested": {"x": 1},
                "nothing": null,
            },
            "bare": "value",
        });
        let extract = |raw: &str| ResultPath::parse(raw).expect(raw).extract(&doc);
        assert_eq!(
            extract("$.scan.newest_created_at").as_deref(),
            Some("2026-08-23T00:00:00Z")
        );
        assert_eq!(extract("$.scan.count").as_deref(), Some("12"));
        assert_eq!(extract("$.scan.empty").as_deref(), Some("false"));
        assert_eq!(extract("$.bare").as_deref(), Some("value"));
        for raw in [
            "$.scan.items",
            "$.scan.nested",
            "$.scan.nothing",
            "$.scan.missing",
            "$.missing.deep",
            "$.scan",
        ] {
            assert_eq!(extract(raw), None, "{raw:?} addresses no scalar");
        }
    }

    /// The [`InputKind::from_parts`] rule, applied to the cursor: stored columns this binary cannot
    /// read degrade to no cursor, so one bad row cannot bail the sweep that fires every other
    /// schedule.
    #[test]
    fn cursor_spec_degrades_unreadable_rows_to_none() {
        assert_eq!(
            CursorSpec::from_columns(Some("$.scan.newest"), Some("since"), None),
            Some(CursorSpec::Field {
                from: ResultPath::parse("$.scan.newest").expect("path"),
                param: "since".to_string(),
            })
        );
        assert_eq!(
            CursorSpec::from_columns(Some("rollup/STATE.json"), None, Some("state.json")),
            Some(CursorSpec::File {
                from: RunFileKey::parse("rollup/STATE.json").expect("key"),
                path: PackPath::parse("state.json").expect("path"),
            })
        );
        for (from, param, path) in [
            (None, None, None),
            (Some("$.scan.newest"), None, None),
            (None, Some("since"), None),
            (Some("not a path"), Some("since"), None),
            (Some("$.scan.newest"), Some("  "), None),
            (Some("$.scan.newest"), Some("since"), Some("state.json")),
            (Some("rollup/STATE.json"), None, None),
            (Some("rollup/STATE.json"), Some("since"), Some("state.json")),
            (Some("rollup/STATE.json"), None, Some("../escape.json")),
            (Some("STATE.json"), None, Some("state.json")),
        ] {
            assert_eq!(
                CursorSpec::from_columns(from, param, path),
                None,
                "{from:?} {param:?} {path:?}"
            );
        }
    }

    /// A write-time build reports which form field was refused, and the grammar of `from` decides
    /// which delivery field it wants.
    #[test]
    fn cursor_spec_parse_names_the_offending_field() {
        let field = |from: &str, param: Option<&str>, path: Option<&str>| {
            CursorSpec::parse(from, param, path).map_err(|(field, _)| field)
        };
        assert_eq!(field("nope", Some("since"), None), Err("cursor.from"));
        assert_eq!(field("$.scan.newest", Some(""), None), Err("cursor.param"));
        assert_eq!(field("$.scan.newest", None, None), Err("cursor.param"));
        assert_eq!(
            field("$.scan.newest", Some("since"), Some("state.json")),
            Err("cursor.path")
        );
        assert_eq!(field("rollup/STATE.json", None, None), Err("cursor.path"));
        assert_eq!(
            field("rollup/STATE.json", Some("since"), None),
            Err("cursor.param")
        );
        for bad in [
            "/abs.json",
            "a/../b.json",
            "./state.json",
            "a//b",
            "a\\b",
            " ",
        ] {
            assert_eq!(
                field("rollup/STATE.json", None, Some(bad)),
                Err("cursor.path"),
                "{bad:?}"
            );
        }
        assert!(field("rollup/STATE.json", None, Some("state/cursor.json")).is_ok());
        assert!(field("rollup[x]/STATE.json", None, Some("state.json")).is_ok());
    }

    /// The pack path resolves under the pack directory and nowhere else.
    #[test]
    fn pack_path_resolves_under_the_pack() {
        let path = PackPath::parse("state/cursor.json").expect("path");
        assert_eq!(
            path.under(std::path::Path::new("/packs/p")),
            std::path::PathBuf::from("/packs/p/state/cursor.json")
        );
    }
}
