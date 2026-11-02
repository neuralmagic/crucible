//! The pack a source belongs to, and the files its path arguments name.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

pub const MANIFEST: &str = "crucible.toml";
pub const ENTRY: &str = "workflow.star";
pub const SKILL_FILE: &str = "SKILL.md";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pack {
    pub dir: PathBuf,
}

/// What a path argument names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    File {
        extension: Option<&'static str>,
    },
    /// A skill directory, which holds a `SKILL.md`.
    Skill,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub dir: bool,
}

impl Pack {
    /// The nearest directory at or above the file holding a crucible.toml.
    pub fn of(file: &Path) -> Option<Pack> {
        file.parent()?
            .ancestors()
            .find(|dir| dir.join(MANIFEST).is_file())
            .map(|dir| Pack {
                dir: dir.to_path_buf(),
            })
    }

    /// A workflow source with no manifest above it is its own pack, as `crucible plan
    /// compile-workflow` treats it.
    pub fn standalone(file: &Path) -> Pack {
        Pack {
            dir: file.parent().unwrap_or(Path::new(".")).to_path_buf(),
        }
    }

    pub fn entry(&self) -> PathBuf {
        self.dir.join(ENTRY)
    }

    /// The `[agent.sandbox.<name>]` tables the manifest declares; `None` without a readable one.
    pub fn sandboxes(&self) -> Option<BTreeSet<String>> {
        let text = std::fs::read_to_string(self.dir.join(MANIFEST)).ok()?;
        let manifest: toml::Table = text.parse().ok()?;
        Some(
            manifest
                .get("agent")
                .and_then(|agent| agent.get("sandbox"))
                .and_then(toml::Value::as_table)
                .map(|sandboxes| sandboxes.keys().cloned().collect())
                .unwrap_or_default(),
        )
    }

    /// The path a pack-relative argument names; `None` when empty, absolute, or using `..`.
    pub fn resolve(&self, raw: &str) -> Option<PathBuf> {
        let path = Path::new(raw);
        let confined = path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
        (!raw.trim().is_empty() && confined).then(|| self.dir.join(path))
    }

    /// Whether `raw` names what `want` asks for.
    pub fn exists(&self, raw: &str, want: Want) -> bool {
        let Some(path) = self.resolve(raw) else {
            return false;
        };
        match want {
            Want::File { .. } => path.is_file(),
            Want::Skill => path.join(SKILL_FILE).is_file(),
        }
    }

    /// The file go-to-definition opens for `raw`.
    pub fn target(&self, raw: &str, want: Want) -> Option<PathBuf> {
        let path = self.resolve(raw)?;
        let path = match want {
            Want::File { .. } => path,
            Want::Skill => path.join(SKILL_FILE),
        };
        path.is_file().then_some(path)
    }

    /// Entries that extend the last segment of `typed`. Hidden ones need a typed `.`; symlinks
    /// never appear.
    pub fn complete(&self, typed: &str, want: Want) -> Vec<Candidate> {
        let (dir, stem) = typed.rsplit_once('/').unwrap_or(("", typed));
        let Some(base) = (if dir.is_empty() {
            Some(self.dir.clone())
        } else {
            self.resolve(dir)
        }) else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(base) else {
            return Vec::new();
        };
        let mut out: Vec<Candidate> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                if !name.starts_with(stem) || (name.starts_with('.') && !stem.starts_with('.')) {
                    return None;
                }
                let kind = entry.file_type().ok()?;
                if kind.is_symlink() {
                    return None;
                }
                let dir = kind.is_dir();
                let wanted = match want {
                    _ if dir => true,
                    Want::File {
                        extension: Some(extension),
                    } => name.ends_with(extension),
                    Want::File { extension: None } => true,
                    Want::Skill => false,
                };
                wanted.then_some(Candidate { name, dir })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

#[cfg(test)]
mod tests {
    use crate::pack::*;

    fn pack() -> (tempfile::TempDir, Pack) {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("pack");
        std::fs::create_dir_all(dir.join("prompts/deep")).expect("dirs");
        std::fs::create_dir_all(dir.join("skills/triage")).expect("dirs");
        std::fs::write(dir.join(MANIFEST), "[agent.sandbox.rust]\nimage = \"x\"\n")
            .expect("manifest");
        std::fs::write(dir.join("prompts/review.md"), "r").expect("prompt");
        std::fs::write(dir.join("prompts/report.json"), "{}").expect("schema");
        std::fs::write(dir.join("prompts/.hidden"), "h").expect("hidden");
        std::fs::write(dir.join("skills/triage/SKILL.md"), "s").expect("skill");
        std::fs::write(dir.join("lib.star"), "x = 1").expect("lib");
        (root, Pack { dir })
    }

    #[test]
    fn a_nested_file_belongs_to_the_nearest_manifest() {
        let (_root, pack) = pack();
        assert_eq!(
            Pack::of(&pack.dir.join("prompts/deep/x.star")),
            Some(pack.clone())
        );
        assert_eq!(
            Pack::of(&pack.dir.join(ENTRY)).map(|p| p.entry()),
            Some(pack.dir.join(ENTRY))
        );
        let elsewhere = tempfile::tempdir().expect("tempdir");
        assert_eq!(Pack::of(&elsewhere.path().join("BUILD.star")), None);
    }

    #[test]
    fn sandboxes_come_from_the_manifest() {
        let (_root, pack) = pack();
        assert_eq!(pack.sandboxes(), Some(BTreeSet::from(["rust".to_string()])));
        let bare = Pack {
            dir: pack.dir.join("prompts"),
        };
        assert_eq!(bare.sandboxes(), None);
    }

    #[test]
    fn paths_that_escape_the_pack_do_not_resolve() {
        let (_root, pack) = pack();
        assert_eq!(pack.resolve("../x"), None);
        assert_eq!(pack.resolve("/etc/passwd"), None);
        assert_eq!(pack.resolve(" "), None);
        assert!(pack.exists("./prompts/review.md", Want::File { extension: None }));
        assert!(!pack.exists("prompts", Want::File { extension: None }));
        assert!(pack.exists("skills/triage", Want::Skill));
        assert!(!pack.exists("skills", Want::Skill));
        assert_eq!(
            pack.target("skills/triage/", Want::Skill),
            Some(pack.dir.join("skills/triage/").join(SKILL_FILE))
        );
    }

    #[test]
    fn completion_lists_the_typed_directory_by_prefix() {
        let (_root, pack) = pack();
        let names = |typed: &str, want: Want| -> Vec<String> {
            pack.complete(typed, want)
                .into_iter()
                .map(|c| c.name)
                .collect()
        };
        let any = Want::File { extension: None };
        assert_eq!(names("prompts/re", any), ["report.json", "review.md"]);
        assert_eq!(
            names(
                "prompts/",
                Want::File {
                    extension: Some(".json")
                }
            ),
            ["deep", "report.json"]
        );
        assert_eq!(names("prompts/.", any), [".hidden"]);
        assert_eq!(
            names(
                "",
                Want::File {
                    extension: Some(".star")
                }
            ),
            ["lib.star", "prompts", "skills"]
        );
        assert_eq!(names("skills/", Want::Skill), ["triage"]);
        assert!(names("../", any).is_empty());
    }
}
