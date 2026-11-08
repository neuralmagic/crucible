//! A workflow pack's files, as the Pack tab lists them: the manifest first, then the workflow
//! source, then everything else.

use std::fs;
use std::path::Path;

/// Files bigger than this are listed by name only.
const MAX_BYTES: u64 = 512 * 1024;
const MAX_DEPTH: usize = 3;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PackFiles {
    /// Where the files came from: a draft version, a Git pin, or a directory.
    pub origin: String,
    pub files: Vec<PackFile>,
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
        PackFiles { origin, files }
    }

    /// A local pack directory, skipping its `state/` and hidden entries.
    pub fn read_dir(dir: &Path) -> Self {
        let mut files = Vec::new();
        walk(dir, dir, 0, &mut files);
        PackFiles::new(dir.display().to_string(), files)
    }

    pub fn file(&self, path: &str) -> Option<&PackFile> {
        self.files.iter().find(|file| file.path == path)
    }
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
}
