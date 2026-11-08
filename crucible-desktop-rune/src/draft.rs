//! Local working copies of controller drafts: a draft version pulled into a directory the user
//! edits in their own editor, and saved back as the next version.
//!
//! ```text
//! <root>/<draft>/        the pack, as the editor and the language server see it
//! <root>/<draft>.json    the version it was pulled or last saved at, and the tree digest then
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crucible_contract::pack_tree::walk_dir;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq)]
pub struct Working {
    pub draft: String,
    pub dir: PathBuf,
    /// The draft version these files were pulled at or last saved as: the base a save carries.
    pub base: i64,
    /// The tree digest at `base`, so an edit shows without asking the controller.
    synced: String,
}

#[derive(Serialize, Deserialize)]
struct Sidecar {
    base_version: i64,
    tree_digest: String,
}

/// Where working copies live: `$CRUCIBLE_DRAFTS_DIR`, else the app's support directory.
pub fn root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CRUCIBLE_DRAFTS_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/crucible-playbooks/drafts"))
}

/// A draft id as one file name.
fn file_name(draft: &str) -> String {
    draft
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The pack's files and its tree digest, read the way a save reads them.
pub fn read(dir: &Path) -> Result<(BTreeMap<String, String>, String), String> {
    let pack = walk_dir(dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
    let digest = pack.tree.digest().to_string();
    let files = pack
        .tree
        .into_files()
        .into_iter()
        .map(|(path, bytes)| {
            let path = path.to_string();
            String::from_utf8(bytes)
                .map(|text| (path.clone(), text))
                .map_err(|_| format!("{path} is not text, so it cannot be saved as a draft"))
        })
        .collect::<Result<_, _>>()?;
    Ok((files, digest))
}

impl Working {
    /// The working copy of `draft` under `root`, when there is one.
    pub fn open(root: &Path, draft: &str) -> Option<Working> {
        let name = file_name(draft);
        let dir = root.join(&name);
        let sidecar: Sidecar =
            serde_json::from_slice(&fs::read(root.join(format!("{name}.json"))).ok()?).ok()?;
        dir.is_dir().then(|| Working {
            draft: draft.to_string(),
            dir,
            base: sidecar.base_version,
            synced: sidecar.tree_digest,
        })
    }

    /// Replace the working copy of `draft` with `files` at `version`.
    pub fn write(
        root: &Path,
        draft: &str,
        version: i64,
        files: &BTreeMap<String, String>,
    ) -> Result<Working, String> {
        let name = file_name(draft);
        let dir = root.join(&name);
        let io = |what: &Path, e: std::io::Error| format!("writing {}: {e}", what.display());
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(|e| io(&dir, e))?;
        }
        fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
        for (path, text) in files {
            let relative = Path::new(path);
            if relative.as_os_str().is_empty()
                || !relative
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
            {
                return Err(format!("{path:?} is not a path inside the pack"));
            }
            let target = dir.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
            }
            fs::write(&target, text).map_err(|e| io(&target, e))?;
        }
        let (_, digest) = read(&dir)?;
        let mut working = Working {
            draft: draft.to_string(),
            dir,
            base: version,
            synced: String::new(),
        };
        working.saved(root, version, digest)?;
        Ok(working)
    }

    /// Record that the files on disk, at `digest`, are now version `version`.
    pub fn saved(&mut self, root: &Path, version: i64, digest: String) -> Result<(), String> {
        let sidecar = root.join(format!("{}.json", file_name(&self.draft)));
        let body = serde_json::to_vec(&Sidecar {
            base_version: version,
            tree_digest: digest.clone(),
        })
        .map_err(|e| e.to_string())?;
        fs::write(&sidecar, body).map_err(|e| format!("writing {}: {e}", sidecar.display()))?;
        self.base = version;
        self.synced = digest;
        Ok(())
    }

    /// Whether the files on disk differ from version `base`.
    pub fn edited(&self) -> bool {
        read(&self.dir).map_or(true, |(_, digest)| digest != self.synced)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::draft::{Working, read};

    fn pack() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("crucible.toml".to_string(), "[workflow]\n".to_string()),
            ("workflow.star".to_string(), "x = 1\n".to_string()),
            ("lib/util.star".to_string(), "y = 2\n".to_string()),
        ])
    }

    #[test]
    fn a_pulled_draft_reopens_at_its_base_and_shows_an_edit() {
        let root = tempfile::tempdir().unwrap();
        let written = Working::write(root.path(), "triage-me", 4, &pack()).unwrap();
        assert!(!written.edited());
        assert_eq!(read(&written.dir).unwrap().0, pack());

        let opened = Working::open(root.path(), "triage-me").unwrap();
        assert_eq!(opened, written);
        assert_eq!(opened.base, 4);

        std::fs::write(opened.dir.join("workflow.star"), "x = 2\n").unwrap();
        assert!(opened.edited());
        std::fs::write(opened.dir.join("workflow.star"), "x = 1\n").unwrap();
        assert!(!opened.edited(), "undoing the edit is no edit");
        std::fs::write(opened.dir.join("notes.md"), "new\n").unwrap();
        assert!(opened.edited(), "a new file is an edit");
    }

    #[test]
    fn a_save_moves_the_base_and_clears_the_edit() {
        let root = tempfile::tempdir().unwrap();
        let mut working = Working::write(root.path(), "triage-me", 4, &pack()).unwrap();
        std::fs::write(working.dir.join("workflow.star"), "x = 3\n").unwrap();
        let (files, digest) = read(&working.dir).unwrap();
        assert_eq!(files["workflow.star"], "x = 3\n");
        working.saved(root.path(), 5, digest).unwrap();
        assert!(!working.edited());
        assert_eq!(Working::open(root.path(), "triage-me").unwrap().base, 5);
    }

    #[test]
    fn a_pull_replaces_the_old_copy_and_refuses_paths_outside_it() {
        let root = tempfile::tempdir().unwrap();
        let working = Working::write(root.path(), "triage-me", 1, &pack()).unwrap();
        std::fs::write(working.dir.join("stray.txt"), "left over\n").unwrap();
        let again = Working::write(
            root.path(),
            "triage-me",
            2,
            &BTreeMap::from([("crucible.toml".to_string(), "".to_string())]),
        )
        .unwrap();
        assert!(!again.dir.join("stray.txt").exists());
        assert!(!again.dir.join("workflow.star").exists());

        for bad in ["../escape", "/abs", ""] {
            let files = BTreeMap::from([(bad.to_string(), "x".to_string())]);
            assert!(
                Working::write(root.path(), "evil", 1, &files).is_err(),
                "{bad:?}"
            );
        }
        assert!(!root.path().join("escape").exists());
        assert!(Working::open(root.path(), "nothing").is_none());
    }
}
