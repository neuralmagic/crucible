//! A playbook pack's files and their `tree1:` digest (RFC-0002:C-PACK-BASE).
//!
//! The digest covers paths and file bytes only, never modes, timestamps, or the archive a pack
//! travels in, so the engine, the controller, and crux name the same pack the same way however
//! it was stored or compressed.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;
use std::path::Path;
use std::str::FromStr;

/// Path segments whose subtree is pod-side runtime or repository metadata, never pack content.
pub const EXCLUDED_SEGMENTS: [&str; 3] = ["state", ".git", "workspace"];

const PREFIX: &str = "tree1:";

/// A pack's `tree1:<hex>` digest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TreeDigest(String);

impl TreeDigest {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TreeDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for TreeDigest {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s
            .strip_prefix(PREFIX)
            .ok_or_else(|| format!("{s:?} is not a tree1: digest"))?;
        if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(format!("{s:?} is not a tree1: digest"));
        }
        Ok(Self(s.to_string()))
    }
}

impl TryFrom<String> for TreeDigest {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<TreeDigest> for String {
    fn from(d: TreeDigest) -> Self {
        d.0
    }
}

/// A pack file's path: relative, `/`-separated, valid UTF-8, with no empty, `.`, or `..`
/// segment and no newline. Ordered byte-wise, which is the order the digest hashes in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PackPath(String);

impl PackPath {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether any segment is one of [`EXCLUDED_SEGMENTS`].
    pub fn is_excluded(&self) -> bool {
        self.0.split('/').any(|s| EXCLUDED_SEGMENTS.contains(&s))
    }
}

impl fmt::Display for PackPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for PackPath {
    type Err = TreeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = |reason: &str| TreeError::InvalidPath {
            path: s.to_string(),
            reason: reason.to_string(),
        };
        if s.contains('\n') {
            return Err(invalid("contains a newline"));
        }
        if s.contains('\\') {
            return Err(invalid("uses a `\\` separator"));
        }
        if s.split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
        {
            return Err(invalid("has an empty, `.`, or `..` segment"));
        }
        Ok(Self(s.to_string()))
    }
}

impl TryFrom<String> for PackPath {
    type Error = TreeError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<PackPath> for String {
    fn from(p: PackPath) -> Self {
        p.0
    }
}

/// Why a set of files is not a pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeError {
    InvalidPath { path: String, reason: String },
    Symlink { path: String },
    NonRegular { path: String },
    PrefixCollision { file: String, under: String },
    Io { path: String, message: String },
}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath { path, reason } => write!(f, "pack path {path:?} {reason}"),
            Self::Symlink { path } => write!(
                f,
                "{path} is a symbolic link; a pack holds regular files only"
            ),
            Self::NonRegular { path } => write!(
                f,
                "{path} is not a regular file; a pack holds regular files only"
            ),
            Self::PrefixCollision { file, under } => {
                write!(
                    f,
                    "{file} is a file, but {under} needs it to be a directory"
                )
            }
            Self::Io { path, message } => write!(f, "reading {path}: {message}"),
        }
    }
}

impl std::error::Error for TreeError {}

/// A pack's files, keyed by path. Excluded paths are never stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackTree(BTreeMap<PackPath, Vec<u8>>);

impl PackTree {
    /// Build a tree from `(path, bytes)` pairs, refusing an excluded path or a file that another
    /// path needs to be a directory.
    pub fn new(files: BTreeMap<PackPath, Vec<u8>>) -> Result<Self, TreeError> {
        if let Some(path) = files.keys().find(|p| p.is_excluded()) {
            return Err(TreeError::InvalidPath {
                path: path.to_string(),
                reason: format!(
                    "is under an excluded segment ({})",
                    EXCLUDED_SEGMENTS.join(", ")
                ),
            });
        }
        let tree = Self(files);
        tree.check_prefix_collisions()?;
        Ok(tree)
    }

    pub fn files(&self) -> &BTreeMap<PackPath, Vec<u8>> {
        &self.0
    }

    pub fn into_files(self) -> BTreeMap<PackPath, Vec<u8>> {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The `tree1:` digest: SHA-256 over one line per file in byte-wise path order, each the
    /// lowercase hex SHA-256 of the file's bytes, two spaces, the path, and a newline.
    pub fn digest(&self) -> TreeDigest {
        let mut summary = Sha256::new();
        for (path, bytes) in &self.0 {
            summary.update(hex(&Sha256::digest(bytes)).as_bytes());
            summary.update(b"  ");
            summary.update(path.as_str().as_bytes());
            summary.update(b"\n");
        }
        TreeDigest(format!("{PREFIX}{}", hex(&summary.finalize())))
    }

    /// Refuse a tree in which a file's path followed by `/` begins another file's path: no
    /// filesystem can hold both.
    fn check_prefix_collisions(&self) -> Result<(), TreeError> {
        for path in self.0.keys() {
            let dir = format!("{}/", path.as_str());
            let under = self
                .0
                .range(PackPath(dir.clone())..)
                .next()
                .map(|(p, _)| p)
                .filter(|p| p.as_str().starts_with(&dir));
            if let Some(under) = under {
                return Err(TreeError::PrefixCollision {
                    file: path.to_string(),
                    under: under.to_string(),
                });
            }
        }
        Ok(())
    }
}

/// A directory read as a pack: the tree, and the excluded paths it skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walked {
    pub tree: PackTree,
    pub ignored: Vec<String>,
}

/// Read the pack rooted at `root`. Excluded segments are skipped at any depth and reported;
/// outside them a symbolic link or other non-regular entry is refused. Links are never followed,
/// and modes and timestamps are not read.
pub fn walk_dir(root: &Path) -> Result<Walked, TreeError> {
    fn walk(
        dir: &Path,
        prefix: &str,
        files: &mut BTreeMap<PackPath, Vec<u8>>,
        ignored: &mut Vec<String>,
    ) -> Result<(), TreeError> {
        let io = |path: &str, e: std::io::Error| TreeError::Io {
            path: path.to_string(),
            message: e.to_string(),
        };
        let shown = if prefix.is_empty() { "." } else { prefix };
        let entries = std::fs::read_dir(dir).map_err(|e| io(shown, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io(shown, e))?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| TreeError::InvalidPath {
                path: format!("{prefix}/{}", name.to_string_lossy()),
                reason: "is not valid UTF-8".to_string(),
            })?;
            let rel = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            if EXCLUDED_SEGMENTS.contains(&name) {
                ignored.push(rel);
                continue;
            }
            let meta = std::fs::symlink_metadata(entry.path()).map_err(|e| io(&rel, e))?;
            let kind = meta.file_type();
            if kind.is_symlink() {
                return Err(TreeError::Symlink { path: rel });
            } else if kind.is_dir() {
                walk(&entry.path(), &rel, files, ignored)?;
            } else if kind.is_file() {
                let bytes = std::fs::read(entry.path()).map_err(|e| io(&rel, e))?;
                files.insert(rel.parse()?, bytes);
            } else {
                return Err(TreeError::NonRegular { path: rel });
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    let mut ignored = Vec::new();
    walk(root, "", &mut files, &mut ignored)?;
    ignored.sort();
    Ok(Walked {
        tree: PackTree::new(files)?,
        ignored,
    })
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Infallible: writing to a String never errors.
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use crate::pack_tree::*;

    fn tree(files: &[(&str, &[u8])]) -> PackTree {
        PackTree::new(
            files
                .iter()
                .map(|(p, b)| (p.parse().expect("path"), b.to_vec()))
                .collect(),
        )
        .expect("tree")
    }

    #[test]
    fn known_answers_pin_the_encoding() {
        assert_eq!(
            tree(&[]).digest().as_str(),
            "tree1:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            tree(&[("workflow.star", b"hello\n")]).digest().as_str(),
            "tree1:89e541a014600dd875ae9135fa79f2a05726e4dce88ccd00a28ff801e65822a6"
        );
        assert_eq!(
            tree(&[("a/b", b"1"), ("a.b", b"2"), ("a-b", b"3")])
                .digest()
                .as_str(),
            "tree1:d9ccd81e116499f2c1d5205318753ce8377bea25b378bd341da447b4a7ed77d4"
        );
    }

    #[test]
    fn digests_round_trip_and_refuse_other_spellings() {
        let d = tree(&[("a", b"x")]).digest();
        assert_eq!(d.as_str().parse::<TreeDigest>().expect("parses"), d);
        let json = serde_json::to_string(&d).expect("serialize");
        assert_eq!(
            serde_json::from_str::<TreeDigest>(&json).expect("deserialize"),
            d
        );
        for bad in [
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "tree1:E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855",
            "tree1:e3b0",
            "tree2:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ] {
            assert!(bad.parse::<TreeDigest>().is_err(), "{bad}");
        }
    }

    #[test]
    fn paths_refuse_what_no_pack_may_hold() {
        for (bad, why) in [
            ("a\nb", "newline"),
            ("a\\b", "`\\`"),
            ("/a", "empty"),
            ("a//b", "empty"),
            ("a/", "empty"),
            ("./a", "`.`"),
            ("a/../b", "`..`"),
        ] {
            let err = bad.parse::<PackPath>().expect_err(bad).to_string();
            assert!(err.contains(why), "{bad}: {err}");
        }
        assert!("tools/measure.sh".parse::<PackPath>().is_ok());
    }

    #[test]
    fn a_file_another_path_needs_as_a_directory_is_refused() {
        let files: BTreeMap<PackPath, Vec<u8>> = [
            ("a", b"1".to_vec()),
            ("a-b", b"2".to_vec()),
            ("a/b", b"3".to_vec()),
        ]
        .into_iter()
        .map(|(p, b)| (p.parse().expect("path"), b))
        .collect();
        assert_eq!(
            PackTree::new(files),
            Err(TreeError::PrefixCollision {
                file: "a".to_string(),
                under: "a/b".to_string()
            })
        );
    }

    #[test]
    fn an_excluded_path_is_not_a_pack_file() {
        let files: BTreeMap<PackPath, Vec<u8>> = [("x/state/y", b"1".to_vec())]
            .into_iter()
            .map(|(p, b)| (p.parse().expect("path"), b))
            .collect();
        assert!(matches!(
            PackTree::new(files),
            Err(TreeError::InvalidPath { .. })
        ));
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pack-tree-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn walking_skips_excluded_segments_at_any_depth_and_reports_them() {
        let root = scratch("excluded");
        for (path, body) in [
            ("crucible.toml", "m"),
            ("tools/run.sh", "r"),
            ("state/session.jsonl", "s"),
            ("x/state/y", "s"),
            ("a/.git/HEAD", "g"),
            ("workspace/repo/file", "w"),
        ] {
            let p = root.join(path);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(&p, body).expect("write");
        }

        let walked = walk_dir(&root).expect("walk");

        let paths: Vec<&str> = walked.tree.files().keys().map(PackPath::as_str).collect();
        assert_eq!(paths, vec!["crucible.toml", "tools/run.sh"]);
        assert_eq!(
            walked.ignored,
            vec!["a/.git", "state", "workspace", "x/state"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_digest_ignores_modes_and_timestamps() {
        use std::os::unix::fs::PermissionsExt;
        let (a, b) = (scratch("mode-a"), scratch("mode-b"));
        for dir in [&a, &b] {
            std::fs::write(dir.join("run.sh"), "#!/bin/sh\n").expect("write");
        }
        std::fs::set_permissions(b.join("run.sh"), std::fs::Permissions::from_mode(0o700))
            .expect("chmod");
        std::fs::File::options()
            .write(true)
            .open(b.join("run.sh"))
            .expect("open")
            .set_modified(std::time::UNIX_EPOCH)
            .expect("touch");

        assert_eq!(
            walk_dir(&a).expect("a").tree.digest(),
            walk_dir(&b).expect("b").tree.digest()
        );
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn walking_refuses_links_and_special_files_outside_excluded_segments() {
        let root = scratch("links");
        std::fs::write(root.join("real"), "x").expect("write");
        std::os::unix::fs::symlink("real", root.join("link")).expect("symlink");
        assert_eq!(
            walk_dir(&root),
            Err(TreeError::Symlink {
                path: "link".to_string()
            })
        );

        std::fs::remove_file(root.join("link")).expect("rm");
        let fifo = root.join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo");
        assert!(made.success());
        assert_eq!(
            walk_dir(&root),
            Err(TreeError::NonRegular {
                path: "pipe".to_string()
            })
        );

        std::fs::remove_file(&fifo).expect("rm");
        std::fs::create_dir_all(root.join(".git")).expect("mkdir");
        std::os::unix::fs::symlink("../real", root.join(".git/link")).expect("symlink");
        assert!(
            walk_dir(&root).is_ok(),
            "links under an excluded segment are skipped"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
