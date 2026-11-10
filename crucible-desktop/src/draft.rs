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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::net::Handle;
use crucible_contract::pack_tree::walk_dir;
use rune::Waker;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::pack::PackFiles;
use crate::preview::Preview;

/// How often a watched working copy is re-read for edits made in the editor.
const REREAD: Duration = Duration::from_secs(1);

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

/// Where working copies live: `$CRUCIBLE_DRAFTS_DIR`, else the app's data directory:
/// `~/Library/Application Support` on macOS, else `$XDG_DATA_HOME` or `~/.local/share`.
pub fn root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CRUCIBLE_DRAFTS_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let base = if cfg!(target_os = "macos") {
        home?.join("Library/Application Support")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".local/share")))?
    };
    Some(base.join("crucible-playbooks/drafts"))
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

/// Where `draft`'s working copy lives under `root`, pulled or not.
pub fn dir(root: &Path, draft: &str) -> PathBuf {
    root.join(file_name(draft))
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
        let dir = dir(root, draft);
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

/// A working copy as last read from disk: its files, whether they are edited, and how they
/// compile.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkingCopy {
    pub working: Working,
    pub pack: PackFiles,
    pub preview: Preview,
    pub edited: bool,
}

impl WorkingCopy {
    /// Read `draft`'s working copy under `root`, when there is one. `before` is the last read:
    /// its compile is kept while the files are the same, and it is returned as is when nothing
    /// moved. Blocking: it reads every file and may compile the pack.
    fn read(
        root: &Path,
        draft: &str,
        before: Option<Arc<WorkingCopy>>,
    ) -> Option<Arc<WorkingCopy>> {
        let working = Working::open(root, draft)?;
        let edited = working.edited();
        let pack = PackFiles {
            origin: format!(
                "working copy of draft {} at version {}{}",
                working.draft,
                working.base,
                if edited { ", edited" } else { "" }
            ),
            ..PackFiles::read_dir(&working.dir)
        };
        let preview = match &before {
            Some(before)
                if before.working.dir == working.dir && before.pack.files == pack.files =>
            {
                before.preview.clone()
            }
            _ => crate::preview::compile(&working.dir),
        };
        let copy = WorkingCopy {
            working,
            pack,
            preview,
            edited,
        };
        match before {
            Some(before) if *before == copy => Some(before),
            _ => Some(Arc::new(copy)),
        }
    }
}

type Published = Arc<Mutex<(u64, Option<Arc<WorkingCopy>>, Option<Waker>)>>;

/// Keeps one draft's working copy read, on the shared runtime. Dropping it ends the task.
pub struct Watcher {
    /// The draft to read, and a count that moves to ask for a read now.
    want: watch::Sender<(Option<String>, u64)>,
    published: Published,
}

impl Watcher {
    /// Read working copies under `root`. With no runtime or no root, nothing is ever read.
    pub fn start(runtime: Result<Handle, String>, root: Option<PathBuf>) -> Watcher {
        let (want, wanted) = watch::channel((None, 0));
        let published = Published::default();
        if let (Ok(runtime), Some(root)) = (runtime, root) {
            runtime.spawn(watch_copy(root, wanted, Arc::clone(&published)));
        }
        Watcher { want, published }
    }

    pub fn set_waker(&self, waker: Waker) {
        if let Ok(mut published) = self.published.lock() {
            published.2 = Some(waker);
        }
    }

    /// Read `draft`'s working copy from now on; `None` stops.
    pub fn watch(&self, draft: Option<&str>) {
        self.want.send_modify(|(wanted, asks)| {
            *wanted = draft.map(str::to_string);
            *asks += 1;
        });
    }

    /// Read the watched copy again now.
    pub fn reread(&self) {
        self.want.send_modify(|(_, asks)| *asks += 1);
    }

    pub fn take(&self, seen: u64) -> Option<(u64, Option<Arc<WorkingCopy>>)> {
        let published = self.published.lock().ok()?;
        (published.0 > seen).then(|| (published.0, published.1.clone()))
    }
}

async fn watch_copy(
    root: PathBuf,
    mut wanted: watch::Receiver<(Option<String>, u64)>,
    published: Published,
) {
    let mut last: Option<Arc<WorkingCopy>> = None;
    loop {
        let (draft, _) = wanted.borrow_and_update().clone();
        let next = match draft {
            Some(draft) => {
                let (root, before) = (root.clone(), last.clone());
                crate::net::blocking(move || WorkingCopy::read(&root, &draft, before))
                    .await
                    .flatten()
            }
            None => None,
        };
        let same = match (&last, &next) {
            (Some(last), Some(next)) => Arc::ptr_eq(last, next),
            (None, None) => true,
            _ => false,
        };
        if !same {
            last = next;
            let waker = match published.lock() {
                Ok(mut published) => {
                    published.0 += 1;
                    published.1 = last.clone();
                    published.2.clone()
                }
                Err(_) => return,
            };
            if let Some(waker) = waker {
                waker();
            }
        }
        tokio::select! {
            changed = wanted.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = crate::net::sleep(REREAD) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::draft::{Watcher, Working, WorkingCopy, read};

    /// The first publish after `seen` that `ready` accepts, within ten seconds.
    fn published(
        watcher: &Watcher,
        seen: &mut u64,
        ready: impl Fn(&Option<Arc<WorkingCopy>>) -> bool,
    ) -> Option<Arc<WorkingCopy>> {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some((version, copy)) = watcher.take(*seen) {
                *seen = version;
                if ready(&copy) {
                    return copy;
                }
            }
            assert!(Instant::now() < until, "no such read was published");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_watched_copy_is_read_off_the_callers_thread_and_follows_edits() {
        let root = tempfile::tempdir().unwrap();
        let written = Working::write(root.path(), "triage-me", 4, &pack()).unwrap();
        Working::write(root.path(), "other", 1, &pack()).unwrap();
        let net = crate::net::Net::uncached_for_tests();
        let watcher = Watcher::start(net.handle(), Some(root.path().to_path_buf()));
        let mut seen = 0;
        assert!(watcher.take(seen).is_none(), "nothing is read until asked");

        watcher.watch(Some("triage-me"));
        let first = published(&watcher, &mut seen, Option::is_some).unwrap();
        assert_eq!(first.working, written);
        assert!(!first.edited);
        assert_eq!(first.pack.files.len(), 3);
        assert!(
            first.pack.origin.ends_with("at version 4"),
            "{}",
            first.pack.origin
        );

        std::thread::sleep(Duration::from_millis(2500));
        assert!(
            watcher.take(seen).is_none(),
            "an unchanged copy is not published again"
        );

        std::fs::write(written.dir.join("workflow.star"), "x = 2\n").unwrap();
        let edited = published(&watcher, &mut seen, |c| {
            c.as_ref().is_some_and(|c| c.edited)
        });
        let edited = edited.unwrap();
        assert!(edited.pack.origin.ends_with(", edited"));
        assert_eq!(
            edited.pack.file("workflow.star").unwrap().text.as_deref(),
            Some("x = 2\n")
        );

        watcher.watch(Some("other"));
        let other = published(&watcher, &mut seen, |c| {
            c.as_ref().is_some_and(|c| c.working.draft == "other")
        });
        assert!(!other.unwrap().edited);

        watcher.watch(Some("never-pulled"));
        assert!(published(&watcher, &mut seen, Option::is_none).is_none());
    }

    #[test]
    fn a_reread_picks_up_a_save_at_once() {
        let root = tempfile::tempdir().unwrap();
        let mut working = Working::write(root.path(), "triage-me", 4, &pack()).unwrap();
        let net = crate::net::Net::uncached_for_tests();
        let watcher = Watcher::start(net.handle(), Some(root.path().to_path_buf()));
        let mut seen = 0;
        watcher.watch(Some("triage-me"));
        published(&watcher, &mut seen, Option::is_some);

        let (_, digest) = read(&working.dir).unwrap();
        working.saved(root.path(), 5, digest).unwrap();
        let asked = Instant::now();
        watcher.reread();
        let saved = published(&watcher, &mut seen, |c| {
            c.as_ref().is_some_and(|c| c.working.base == 5)
        });
        assert!(
            asked.elapsed() < Duration::from_millis(900),
            "{:?}",
            asked.elapsed()
        );
        assert!(saved.unwrap().pack.origin.ends_with("at version 5"));
    }

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
            &BTreeMap::from([("crucible.toml".to_string(), String::new())]),
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
