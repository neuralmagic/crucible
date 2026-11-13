//! A draft's save history read as changes: which files each save touched, and the unified diff
//! between any two saves. Read-only over `playbook_draft_versions` and the pack tree store; the
//! caller has already decided the reader may see the draft.

#![allow(clippy::disallowed_macros)]

use crate::playbooks::drafts::{DraftError, DraftVersionRow};
use anyhow::{Context, Result};
use crucible_contract::pack_tree::PackTree;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// How long one diff may spend looking for a minimal edit script before it settles for a valid,
/// coarser one.
const DIFF_BUDGET: Duration = Duration::from_secs(5);

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    Added,
    Removed,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub change: FileChange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub version: DraftVersionRow,
    /// The save this one is compared against; `None` for the first, which is compared against an
    /// empty pack.
    pub previous: Option<i64>,
    /// `None` when this save or the previous one has no stored tree to compare.
    pub changes: Option<Vec<ChangedFile>>,
}

pub async fn history(pool: &PgPool, id: &str) -> Result<Vec<HistoryEntry>> {
    let rows = sqlx::query(
        r#"WITH v AS (
               SELECT version, tree_digest, schema_digest, diagnostics, core_rev, created_by,
                      created_at,
                      LAG(version) OVER w AS previous,
                      LAG(tree_digest) OVER w AS previous_digest
               FROM playbook_draft_versions WHERE draft_id = $1
               WINDOW w AS (ORDER BY version)
           )
           SELECT v.version, v.tree_digest, v.schema_digest, v.diagnostics, v.core_rev,
                  v.created_by, v.created_at, v.previous,
                  CASE WHEN v.tree_digest IS NOT NULL
                            AND (v.previous IS NULL OR v.previous_digest IS NOT NULL)
                  THEN COALESCE((
                      SELECT jsonb_agg(jsonb_build_object(
                          'path', COALESCE(n.path, o.path),
                          'change', CASE WHEN o.path IS NULL THEN 'added'
                                         WHEN n.path IS NULL THEN 'removed'
                                         ELSE 'modified' END))
                      FROM (SELECT path, sha256 FROM pack_tree_files
                            WHERE digest = v.tree_digest) n
                      FULL JOIN (SELECT path, sha256 FROM pack_tree_files
                                 WHERE digest = v.previous_digest) o ON o.path = n.path
                      WHERE n.sha256 IS DISTINCT FROM o.sha256
                  ), '[]'::jsonb) END AS changes
           FROM v ORDER BY v.version DESC"#,
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .context("reading a draft's history")?;
    rows.iter()
        .map(|row| {
            let changes: Option<serde_json::Value> = row.try_get("changes")?;
            let mut changes: Option<Vec<ChangedFile>> = changes
                .map(serde_json::from_value)
                .transpose()
                .context("decoding a draft version's changed files")?;
            if let Some(changes) = &mut changes {
                changes.sort();
            }
            Ok(HistoryEntry {
                version: DraftVersionRow::from_row(row)?,
                previous: row.try_get("previous")?,
                changes,
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub change: FileChange,
    pub additions: usize,
    pub deletions: usize,
    /// The unified diff, `a/` the older side; `None` when either side is not text.
    pub unified: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionDiff {
    /// `None` when `to` is the draft's first save, diffed against an empty pack.
    pub from: Option<i64>,
    pub to: i64,
    pub files: Vec<FileDiff>,
}

/// The diff from save `from` to save `to`. `to` defaults to the newest save and `from` to the
/// save before `to`. `None` when the draft or `to` is unknown; a `from` the draft does not hold is
/// [`NotFound`](DraftError::NotFound).
pub async fn diff(
    pool: &PgPool,
    id: &str,
    from: Option<i64>,
    to: Option<i64>,
) -> Result<Option<VersionDiff>, DraftError> {
    let Some((to, after)) = crate::playbooks::drafts::version_tree(pool, id, to).await? else {
        return Ok(None);
    };
    let from = match from {
        Some(from) => Some(from),
        None => sqlx::query_scalar(
            "SELECT max(version) FROM playbook_draft_versions WHERE draft_id = $1 AND version < $2",
        )
        .bind(id)
        .bind(to)
        .fetch_one(pool)
        .await
        .context("finding the save before a draft version")?,
    };
    let before = match from {
        Some(from) => Some(
            crate::playbooks::drafts::version_tree(pool, id, Some(from))
                .await?
                .ok_or_else(|| DraftError::NotFound(format!("draft {id} has no version {from}")))?
                .1,
        ),
        None => None,
    };
    let files = tokio::task::spawn_blocking(move || diff_trees(before.as_ref(), &after))
        .await
        .context("joining the draft diff worker")?;
    Ok(Some(VersionDiff { from, to, files }))
}

fn diff_trees(before: Option<&PackTree>, after: &PackTree) -> Vec<FileDiff> {
    let deadline = Instant::now() + DIFF_BUDGET;
    let empty = std::collections::BTreeMap::new();
    let old = before.map_or(&empty, PackTree::files);
    let new = after.files();
    let paths: BTreeSet<_> = old.keys().chain(new.keys()).collect();
    paths
        .into_iter()
        .filter_map(|path| {
            let (change, a, b) = match (old.get(path), new.get(path)) {
                (Some(a), Some(b)) if a == b => return None,
                (Some(a), Some(b)) => (FileChange::Modified, a.as_slice(), b.as_slice()),
                (None, Some(b)) => (FileChange::Added, &[][..], b.as_slice()),
                (Some(a), None) => (FileChange::Removed, a.as_slice(), &[][..]),
                (None, None) => return None,
            };
            Some(diff_file(path.as_str(), change, a, b, deadline))
        })
        .collect()
}

fn diff_file(
    path: &str,
    change: FileChange,
    old: &[u8],
    new: &[u8],
    deadline: Instant,
) -> FileDiff {
    let (Ok(old), Ok(new)) = (std::str::from_utf8(old), std::str::from_utf8(new)) else {
        return FileDiff {
            path: path.to_string(),
            change,
            additions: 0,
            deletions: 0,
            unified: None,
        };
    };
    let diff = similar::TextDiff::configure()
        .deadline(deadline)
        .diff_lines(old, new);
    let count = |tag| diff.iter_all_changes().filter(|c| c.tag() == tag).count();
    let (a, b) = match change {
        FileChange::Added => ("/dev/null".to_string(), format!("b/{path}")),
        FileChange::Removed => (format!("a/{path}"), "/dev/null".to_string()),
        FileChange::Modified => (format!("a/{path}"), format!("b/{path}")),
    };
    FileDiff {
        path: path.to_string(),
        change,
        additions: count(similar::ChangeTag::Insert),
        deletions: count(similar::ChangeTag::Delete),
        unified: Some(diff.unified_diff().header(&a, &b).to_string()),
    }
}

#[cfg(test)]
mod tests {
    use crate::playbooks::draft_history::*;
    use crate::playbooks::drafts::{
        DraftSeed, SKELETON_MANIFEST, SKELETON_WORKFLOW, create, save_version,
    };
    use std::collections::BTreeMap;

    fn skeleton() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("crucible.toml".to_string(), SKELETON_MANIFEST.to_string()),
            ("workflow.star".to_string(), SKELETON_WORKFLOW.to_string()),
        ])
    }

    async fn studio(pool: &PgPool) {
        create(
            pool,
            "studio",
            "a drafted pack",
            DraftSeed::Skeleton,
            Some("wren"),
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("the skeleton compiles");
    }

    fn changed(path: &str, change: FileChange) -> ChangedFile {
        ChangedFile {
            path: path.to_string(),
            change,
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn history_lists_each_save_with_the_files_it_changed(pool: PgPool) {
        studio(&pool).await;
        let mut files = skeleton();
        files.insert("skills/review.md".to_string(), "review\n".to_string());
        files.insert(
            "workflow.star".to_string(),
            SKELETON_WORKFLOW.replace("echo hello", "echo hi"),
        );
        save_version(&pool, "studio", files.clone(), Some("ada"), Some(1))
            .await
            .expect("save 2");
        save_version(&pool, "studio", files.clone(), Some("ada"), Some(2))
            .await
            .expect("save 3");
        files.remove("skills/review.md");
        save_version(&pool, "studio", files, Some("wren"), Some(3))
            .await
            .expect("save 4");

        let history = history(&pool, "studio").await.expect("history");
        let seen: Vec<_> = history
            .iter()
            .map(|e| (e.version.version, e.previous, e.changes.clone()))
            .collect();
        assert_eq!(
            seen,
            vec![
                (
                    4,
                    Some(3),
                    Some(vec![changed("skills/review.md", FileChange::Removed)])
                ),
                (3, Some(2), Some(vec![])),
                (
                    2,
                    Some(1),
                    Some(vec![
                        changed("skills/review.md", FileChange::Added),
                        changed("workflow.star", FileChange::Modified),
                    ])
                ),
                (
                    1,
                    None,
                    Some(vec![
                        changed("crucible.toml", FileChange::Added),
                        changed("workflow.star", FileChange::Added),
                    ])
                ),
            ]
        );
        assert_eq!(history[0].version.created_by.as_deref(), Some("wren"));
        assert_eq!(history[2].version.created_by.as_deref(), Some("ada"));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn history_of_an_unknown_draft_is_empty(pool: PgPool) {
        assert!(history(&pool, "nope").await.expect("history").is_empty());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_save_without_a_stored_tree_has_unknown_changes_and_so_does_the_one_after(
        pool: PgPool,
    ) {
        studio(&pool).await;
        let mut files = skeleton();
        for n in 2..=3 {
            files.insert("notes.md".to_string(), format!("save {n}\n"));
            save_version(&pool, "studio", files.clone(), None, None)
                .await
                .expect("save");
        }
        sqlx::query(
            "UPDATE playbook_draft_versions SET tree_digest = NULL
             WHERE draft_id = 'studio' AND version = 2",
        )
        .execute(&pool)
        .await
        .expect("drop the tree pin");

        let history = history(&pool, "studio").await.expect("history");
        let known: Vec<_> = history
            .iter()
            .map(|e| (e.version.version, e.changes.is_some()))
            .collect();
        assert_eq!(known, vec![(3, false), (2, false), (1, true)]);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn diff_defaults_to_the_newest_save_against_the_one_before(pool: PgPool) {
        studio(&pool).await;
        let mut files = skeleton();
        files.insert(
            "workflow.star".to_string(),
            SKELETON_WORKFLOW.replace("echo hello", "echo hi"),
        );
        files.insert("skills/review.md".to_string(), "review\n".to_string());
        save_version(&pool, "studio", files, None, Some(1))
            .await
            .expect("save 2");

        let diff = diff(&pool, "studio", None, None)
            .await
            .expect("diff")
            .expect("the draft exists");
        assert_eq!((diff.from, diff.to), (Some(1), 2));
        let [added, modified] = diff.files.as_slice() else {
            panic!("expected two changed files, got {:?}", diff.files);
        };
        assert_eq!(
            added,
            &FileDiff {
                path: "skills/review.md".to_string(),
                change: FileChange::Added,
                additions: 1,
                deletions: 0,
                unified: Some(
                    "--- /dev/null\n+++ b/skills/review.md\n@@ -0,0 +1 @@\n+review\n".to_string()
                ),
            }
        );
        assert_eq!(
            (modified.path.as_str(), modified.change),
            ("workflow.star", FileChange::Modified)
        );
        assert_eq!((modified.additions, modified.deletions), (1, 1));
        let unified = modified.unified.as_deref().expect("text");
        assert!(
            unified.starts_with("--- a/workflow.star\n+++ b/workflow.star\n@@ "),
            "{unified}"
        );
        assert!(
            unified.contains(
                "\n-hello = command(name = \"hello\", run = \"echo hello\")\n\
                 +hello = command(name = \"hello\", run = \"echo hi\")\n"
            ),
            "{unified}"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn the_first_save_diffs_against_an_empty_pack(pool: PgPool) {
        studio(&pool).await;
        let diff = diff(&pool, "studio", None, Some(1))
            .await
            .expect("diff")
            .expect("the draft exists");
        assert_eq!((diff.from, diff.to), (None, 1));
        let changes: Vec<_> = diff
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.change, f.deletions))
            .collect();
        assert_eq!(
            changes,
            vec![
                ("crucible.toml", FileChange::Added, 0),
                ("workflow.star", FileChange::Added, 0),
            ]
        );
        assert_eq!(
            diff.files[1].additions,
            SKELETON_WORKFLOW.lines().count(),
            "every line of a new file is an addition"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn diff_spans_any_two_saves_in_either_direction(pool: PgPool) {
        studio(&pool).await;
        let mut files = skeleton();
        for n in 2..=4 {
            files.insert("notes.md".to_string(), format!("save {n}\n"));
            save_version(&pool, "studio", files.clone(), None, None)
                .await
                .expect("save");
        }

        let forward = diff(&pool, "studio", Some(2), Some(4))
            .await
            .expect("diff")
            .expect("the draft exists");
        assert_eq!((forward.from, forward.to), (Some(2), 4));
        let [notes] = forward.files.as_slice() else {
            panic!("expected one changed file, got {:?}", forward.files);
        };
        assert_eq!(
            notes.unified.as_deref(),
            Some("--- a/notes.md\n+++ b/notes.md\n@@ -1 +1 @@\n-save 2\n+save 4\n")
        );

        let backward = diff(&pool, "studio", Some(4), Some(1))
            .await
            .expect("diff")
            .expect("the draft exists");
        let [notes] = backward.files.as_slice() else {
            panic!("expected one changed file, got {:?}", backward.files);
        };
        assert_eq!(notes.change, FileChange::Removed);
        assert_eq!(
            notes.unified.as_deref(),
            Some("--- a/notes.md\n+++ /dev/null\n@@ -1 +0,0 @@\n-save 4\n")
        );

        let same = diff(&pool, "studio", Some(3), Some(3))
            .await
            .expect("diff")
            .expect("the draft exists");
        assert!(same.files.is_empty(), "{:?}", same.files);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn diff_refuses_a_version_the_draft_does_not_hold(pool: PgPool) {
        studio(&pool).await;
        assert!(
            diff(&pool, "nope", None, None)
                .await
                .expect("diff")
                .is_none()
        );
        assert!(
            diff(&pool, "studio", None, Some(9))
                .await
                .expect("diff")
                .is_none()
        );
        match diff(&pool, "studio", Some(9), Some(1)).await {
            Err(DraftError::NotFound(msg)) => {
                assert_eq!(msg, "draft studio has no version 9")
            }
            other => panic!("expected a not-found refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_file_that_is_not_text_is_reported_without_a_unified_diff() {
        let before =
            PackTree::from_pairs(&[("blob", &[0xff, 0xfe]), ("same.md", b"same\n")]).expect("tree");
        let after =
            PackTree::from_pairs(&[("blob", &[0xff, 0xfd]), ("same.md", b"same\n")]).expect("tree");
        assert_eq!(
            diff_trees(Some(&before), &after),
            vec![FileDiff {
                path: "blob".to_string(),
                change: FileChange::Modified,
                additions: 0,
                deletions: 0,
                unified: None,
            }]
        );
    }

    #[test]
    fn a_missing_trailing_newline_is_marked_in_the_unified_diff() {
        let before = PackTree::from_pairs(&[("a.md", b"one\n")]).expect("tree");
        let after = PackTree::from_pairs(&[("a.md", b"one\ntwo")]).expect("tree");
        let [file] = &diff_trees(Some(&before), &after)[..] else {
            panic!("expected one changed file");
        };
        assert_eq!(
            file.unified.as_deref(),
            Some(
                "--- a/a.md\n+++ b/a.md\n@@ -1 +1,2 @@\n one\n+two\n\\ No newline at end of file\n"
            )
        );
    }
}
