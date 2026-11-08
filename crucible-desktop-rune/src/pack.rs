//! A workflow pack's files, as the Pack tab lists them: the manifest first, then the workflow
//! source, then everything else.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// Files bigger than this are listed by name only.
const MAX_BYTES: u64 = 512 * 1024;
const MAX_DEPTH: usize = 3;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PackFiles {
    /// Where the files came from: a draft version, a Git pin, or a directory.
    pub origin: String,
    pub files: Vec<PackFile>,
    /// Where the files are on disk, for the language server and the editor.
    pub disk: Option<Disk>,
    /// The pack's directory on the Git host at the pinned commit, ending in `/`.
    pub web: Option<String>,
}

/// A pack's directory: the checkout it was read from, or a copy of a controller's pack.
#[derive(Clone, Debug, PartialEq)]
pub enum Disk {
    Checkout(PathBuf),
    Copy(PathBuf),
}

impl Disk {
    pub fn dir(&self) -> &Path {
        match self {
            Disk::Checkout(dir) | Disk::Copy(dir) => dir,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PackFile {
    pub path: String,
    /// `None` for a file too big or not text.
    pub text: Option<String>,
}

impl PackFiles {
    pub fn new(origin: String, files: impl IntoIterator<Item = (String, Option<String>)>) -> Self {
        let mut files: Vec<PackFile> = files
            .into_iter()
            .map(|(path, text)| PackFile { path, text })
            .collect();
        files.sort_by(|a, b| rank(&a.path).cmp(&rank(&b.path)).then(a.path.cmp(&b.path)));
        PackFiles {
            origin,
            files,
            disk: None,
            web: None,
        }
    }

    /// A local pack directory, skipping its `state/` and hidden entries.
    pub fn read_dir(dir: &Path) -> Self {
        let mut files = Vec::new();
        walk(dir, dir, 0, &mut files);
        PackFiles {
            disk: Some(Disk::Checkout(dir.to_path_buf())),
            ..PackFiles::new(dir.display().to_string(), files)
        }
    }

    /// Write the text files to `dir`, replacing what was there, and remember it as their copy.
    /// A path that would leave `dir` is skipped.
    pub fn copy_to(&mut self, dir: &Path) -> std::io::Result<()> {
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        for file in &self.files {
            let Some(text) = &file.text else {
                continue;
            };
            let relative = Path::new(&file.path);
            if !relative
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            {
                continue;
            }
            let target = dir.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(target, text)?;
        }
        self.disk = Some(Disk::Copy(dir.to_path_buf()));
        Ok(())
    }

    /// Pin the files to `repo` (`owner/name` or a URL) at commit `rev`, under `path`.
    pub fn on_git(&mut self, repo: &str, rev: &str, path: &str) {
        let repo = repo.trim().trim_end_matches('/').trim_end_matches(".git");
        if repo.is_empty() || rev.is_empty() {
            return;
        }
        let host = if repo.contains("://") {
            repo.to_string()
        } else {
            format!("https://github.com/{repo}")
        };
        let path = path.trim_matches('/');
        let dir = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        self.web = Some(format!("{host}/blob/{rev}/{dir}"));
    }

    /// `file` on the Git host, at 1-based `line`.
    pub fn web_url(&self, file: &str, line: usize) -> Option<String> {
        let base = self.web.as_ref()?;
        Some(format!("{base}{file}#L{line}"))
    }

    /// The file's path on disk.
    pub fn path_of(&self, file: &str) -> Option<PathBuf> {
        Some(self.disk.as_ref()?.dir().join(file))
    }

    pub fn file(&self, path: &str) -> Option<&PackFile> {
        self.files.iter().find(|file| file.path == path)
    }
}

/// Where copies of controller packs go: the user's cache directory.
pub fn cache_dir(key: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let name: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(
        PathBuf::from(home)
            .join("Library/Caches/crucible-playbooks/packs")
            .join(name),
    )
}

/// `crucible.toml`, then the plan and workflow source, then the rest.
fn rank(path: &str) -> u8 {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name {
        "crucible.toml" if !path.contains('/') => 0,
        "plan.toml" if !path.contains('/') => 1,
        _ if name.ends_with(".star") || name.ends_with(".bzl") => 2,
        _ if name.ends_with(".toml") => 3,
        _ => 4,
    }
}

fn walk(root: &Path, dir: &Path, depth: usize, files: &mut Vec<(String, Option<String>)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || (depth == 0 && name == "state") {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth + 1 < MAX_DEPTH {
                walk(root, &path, depth + 1, files);
            }
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let small = entry.metadata().is_ok_and(|meta| meta.len() <= MAX_BYTES);
        let text = small
            .then(|| fs::read(&path).ok())
            .flatten()
            .and_then(|bytes| String::from_utf8(bytes).ok());
        files.push((relative, text));
    }
}

#[cfg(test)]
mod tests {
    use crate::pack::PackFiles;
    use std::fs;

    #[test]
    fn a_git_pin_links_each_file_and_line_at_its_commit() {
        let mut pack = PackFiles::new("pin".into(), []);
        assert_eq!(pack.web_url("workflow.star", 3), None);
        pack.on_git("neuralmagic/crucible", "091a008", "examples/triage/");
        assert_eq!(
            pack.web_url("workflow.star", 12).as_deref(),
            Some(
                "https://github.com/neuralmagic/crucible/blob/091a008/examples/triage/workflow.star#L12"
            )
        );
        pack.on_git("https://gitlab.example.com/team/packs.git", "abc", "");
        assert_eq!(
            pack.web_url("lib/x.star", 1).as_deref(),
            Some("https://gitlab.example.com/team/packs/blob/abc/lib/x.star#L1")
        );
        let mut unpinned = PackFiles::new("pin".into(), []);
        unpinned.on_git("neuralmagic/crucible", "", "examples/triage");
        assert_eq!(unpinned.web, None);
    }

    #[test]
    fn the_manifest_leads_then_the_plan_and_workflow_source() {
        let pack = PackFiles::new(
            "draft v5".into(),
            [
                ("README.md".to_string(), Some(String::new())),
                ("lib/helpers.star".into(), Some(String::new())),
                ("workflow.star".into(), Some(String::new())),
                ("crucible.toml".into(), Some(String::new())),
                ("plan.toml".into(), Some(String::new())),
                ("tools/crucible.toml".into(), Some(String::new())),
            ],
        );
        let order: Vec<&str> = pack.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            order,
            [
                "crucible.toml",
                "plan.toml",
                "lib/helpers.star",
                "workflow.star",
                "tools/crucible.toml",
                "README.md",
            ]
        );
        assert!(pack.file("workflow.star").is_some());
        assert!(pack.file("nope").is_none());
    }

    #[test]
    fn a_local_pack_reads_its_files_but_not_its_state_or_hidden_ones() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("crucible.toml"),
            "[workflow]\ntype = \"playbook\"\n",
        )
        .unwrap();
        fs::write(root.join("workflow.star"), "def main(): pass\n").unwrap();
        fs::create_dir_all(root.join("state")).unwrap();
        fs::write(root.join("state/session.jsonl"), "{}\n").unwrap();
        fs::write(root.join(".hidden"), "x").unwrap();
        fs::create_dir_all(root.join("prompts")).unwrap();
        fs::write(root.join("prompts/edit.md"), "do the thing").unwrap();
        fs::write(root.join("blob.bin"), [0xff, 0xfe, 0x00]).unwrap();

        let pack = PackFiles::read_dir(root);
        let order: Vec<&str> = pack.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            order,
            [
                "crucible.toml",
                "workflow.star",
                "blob.bin",
                "prompts/edit.md"
            ]
        );
        assert_eq!(
            pack.file("workflow.star").and_then(|f| f.text.as_deref()),
            Some("def main(): pass\n")
        );
        assert_eq!(pack.file("blob.bin").map(|f| f.text.is_none()), Some(true));
    }

    #[test]
    fn a_copy_writes_the_text_files_and_nothing_outside() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pack");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("stale.star"), "old").unwrap();
        let mut pack = PackFiles::new(
            "draft v2".into(),
            [
                ("workflow.star".to_string(), Some("x = 1".to_string())),
                ("lib/a.star".into(), Some("y = 2".into())),
                ("blob.bin".into(), None),
                ("../escape.star".into(), Some("no".into())),
                ("/abs.star".into(), Some("no".into())),
            ],
        );
        pack.copy_to(&target).unwrap();
        assert_eq!(
            fs::read_to_string(target.join("lib/a.star")).unwrap(),
            "y = 2"
        );
        assert_eq!(
            pack.path_of("workflow.star"),
            Some(target.join("workflow.star"))
        );
        assert!(matches!(pack.disk, Some(crate::pack::Disk::Copy(_))));
        for gone in ["stale.star", "blob.bin"] {
            assert!(!target.join(gone).exists(), "{gone}");
        }
        assert!(!dir.path().join("escape.star").exists());
    }

    #[test]
    fn a_checkout_is_its_own_disk_and_cache_names_are_plain() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("workflow.star"), "x = 1").unwrap();
        let pack = PackFiles::read_dir(dir.path());
        assert_eq!(
            pack.disk,
            Some(crate::pack::Disk::Checkout(dir.path().to_path_buf()))
        );
        let cache = crate::pack::cache_dir("triage@draft-3/x").unwrap();
        assert!(cache.ends_with("packs/triage_draft-3_x"), "{cache:?}");
    }
}
