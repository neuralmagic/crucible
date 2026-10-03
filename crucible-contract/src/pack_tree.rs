//! A playbook pack's files, their `tree1:` digest (RFC-0002:C-PACK-BASE), and the one archive
//! encoding a run receives them in.
//!
//! The digest covers paths and file bytes only, never modes, timestamps, or the archive a pack
//! travels in, so the engine, the controller, and crux name the same pack the same way however
//! it was stored or compressed.

use crate::artifact::{lower_hex, sha256_hex};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path};
use std::str::FromStr;

/// Path segments whose subtree is pod-side runtime or repository metadata, never pack content.
pub const EXCLUDED_SEGMENTS: [&str; 3] = ["state", ".git", "workspace"];

/// The most gzipped bytes a pack may deliver to a run: its [`PackTree::tarball`] rides one
/// ConfigMap key, and Kubernetes caps a ConfigMap at 1 MiB of decoded values.
pub const DELIVERY_BUDGET_BYTES: usize = 900 * 1024;

/// The most bytes [`read_tar_gz`] will gunzip: a pack within the delivery budget expands far less,
/// and an archive past it is refused before it is held in memory.
pub const MAX_EXPANDED_BYTES: u64 = 64 * 1024 * 1024;

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

/// A file's path inside a pack: relative, `/`-separated, valid UTF-8, with no empty, `.`, or
/// `..` segment and no newline. Ordered byte-wise, which is the order the digest hashes in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PackFilePath(String);

impl PackFilePath {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether any segment is one of [`EXCLUDED_SEGMENTS`].
    pub fn is_excluded(&self) -> bool {
        self.0.split('/').any(|s| EXCLUDED_SEGMENTS.contains(&s))
    }
}

impl fmt::Display for PackFilePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for PackFilePath {
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

impl TryFrom<String> for PackFilePath {
    type Error = TreeError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<PackFilePath> for String {
    fn from(p: PackFilePath) -> Self {
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
    Expands { limit: u64 },
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
            Self::Expands { limit } => {
                write!(f, "the pack archive expands past {limit} bytes")
            }
        }
    }
}

impl std::error::Error for TreeError {}

/// A pack whose [`PackTree::tarball`] is over [`DELIVERY_BUDGET_BYTES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverBudget {
    pub bytes: usize,
}

impl fmt::Display for OverBudget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the pack delivers {} gzipped bytes to a run, over the {DELIVERY_BUDGET_BYTES}-byte delivery budget",
            self.bytes
        )
    }
}

impl std::error::Error for OverBudget {}

/// Refuse a delivered tarball over [`DELIVERY_BUDGET_BYTES`].
pub fn check_delivery_budget(tarball: &[u8]) -> Result<(), OverBudget> {
    if tarball.len() > DELIVERY_BUDGET_BYTES {
        return Err(OverBudget {
            bytes: tarball.len(),
        });
    }
    Ok(())
}

/// A pack's files, keyed by path. Excluded paths are never stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackTree(BTreeMap<PackFilePath, Vec<u8>>);

impl PackTree {
    /// Build a tree, refusing an excluded path or a file that another path needs to be a
    /// directory.
    pub fn new(files: BTreeMap<PackFilePath, Vec<u8>>) -> Result<Self, TreeError> {
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

    /// Build a tree from `(path, bytes)` pairs.
    pub fn from_pairs(pairs: &[(&str, &[u8])]) -> Result<Self, TreeError> {
        Self::new(
            pairs
                .iter()
                .map(|(path, bytes)| Ok((path.parse()?, bytes.to_vec())))
                .collect::<Result<_, TreeError>>()?,
        )
    }

    pub fn files(&self) -> &BTreeMap<PackFilePath, Vec<u8>> {
        &self.0
    }

    pub fn into_files(self) -> BTreeMap<PackFilePath, Vec<u8>> {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The `tree1:` digest: SHA-256 over one line per file in byte-wise path order, each the
    /// lowercase hex SHA-256 of the file's bytes, two spaces, the path, and a newline.
    pub fn digest(&self) -> TreeDigest {
        self.digest_with_file_hashes().0
    }

    /// The digest, and each file's lowercase hex SHA-256 in path order, hashed once.
    pub fn digest_with_file_hashes(&self) -> (TreeDigest, Vec<String>) {
        let mut summary = Sha256::new();
        let mut file_hashes = Vec::with_capacity(self.0.len());
        for (path, bytes) in &self.0 {
            let file_sha = sha256_hex(bytes);
            summary.update(file_sha.as_bytes());
            summary.update(b"  ");
            summary.update(path.as_str().as_bytes());
            summary.update(b"\n");
            file_hashes.push(file_sha);
        }
        (
            TreeDigest(format!("{PREFIX}{}", lower_hex(&summary.finalize()))),
            file_hashes,
        )
    }

    /// The gzipped tar a run receives: entries in path order, each a regular file with mode
    /// 0755, owner 0, and mtime 0, in a gzip stream with no name or timestamp. The same tree
    /// always yields the same bytes.
    pub fn tarball(&self) -> std::io::Result<Vec<u8>> {
        let gz = flate2::GzBuilder::new().write(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(gz);
        for (path, bytes) in &self.0 {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            builder.append_data(&mut header, path.as_str(), bytes.as_slice())?;
        }
        builder.into_inner()?.finish()
    }

    /// Refuse a tree in which a file's path followed by `/` begins another file's path: no
    /// filesystem can hold both.
    fn check_prefix_collisions(&self) -> Result<(), TreeError> {
        for path in self.0.keys() {
            let dir = format!("{}/", path.as_str());
            let under = self
                .0
                .range(PackFilePath(dir.clone())..)
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

/// A pack read from a directory or an archive: the tree, and the excluded paths it skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPack {
    pub tree: PackTree,
    pub ignored: Vec<String>,
}

/// What one entry of a directory or archive is, as far as a pack cares.
enum EntryKind {
    Dir,
    File(Vec<u8>),
    Symlink,
    Other,
}

/// Collects a pack from entries in any order, applying the one set of rules both readers share:
/// excluded segments are skipped at any depth and reported, and outside them a symlink or other
/// non-regular entry is refused. A later entry for a path replaces an earlier one.
#[derive(Default)]
struct Collector {
    files: BTreeMap<PackFilePath, Vec<u8>>,
    ignored: BTreeSet<String>,
}

impl Collector {
    /// Admit one entry. Returns whether a directory entry should be descended into.
    fn admit(&mut self, segments: &[&str], kind: EntryKind) -> Result<bool, TreeError> {
        if segments.is_empty() {
            return Ok(true);
        }
        if let Some(i) = segments.iter().position(|s| EXCLUDED_SEGMENTS.contains(s)) {
            self.ignored.insert(segments[..=i].join("/"));
            return Ok(false);
        }
        let rel = segments.join("/");
        match kind {
            EntryKind::Dir => Ok(true),
            EntryKind::File(bytes) => {
                self.files.insert(rel.parse()?, bytes);
                Ok(false)
            }
            EntryKind::Symlink => Err(TreeError::Symlink { path: rel }),
            EntryKind::Other => Err(TreeError::NonRegular { path: rel }),
        }
    }

    fn finish(self) -> Result<ReadPack, TreeError> {
        Ok(ReadPack {
            tree: PackTree::new(self.files)?,
            ignored: self.ignored.into_iter().collect(),
        })
    }
}

/// Read the pack rooted at `root`. Links are never followed, and modes and timestamps are not
/// read.
pub fn walk_dir(root: &Path) -> Result<ReadPack, TreeError> {
    fn walk(dir: &Path, prefix: &[&str], pack: &mut Collector) -> Result<(), TreeError> {
        let io = |path: String, e: std::io::Error| TreeError::Io {
            path,
            message: e.to_string(),
        };
        let shown = || {
            if prefix.is_empty() {
                ".".to_string()
            } else {
                prefix.join("/")
            }
        };
        for entry in std::fs::read_dir(dir).map_err(|e| io(shown(), e))? {
            let entry = entry.map_err(|e| io(shown(), e))?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| TreeError::InvalidPath {
                path: format!("{}/{}", shown(), name.to_string_lossy()),
                reason: "is not valid UTF-8".to_string(),
            })?;
            let segments: Vec<&str> = prefix.iter().copied().chain([name]).collect();
            let rel = segments.join("/");
            let path = entry.path();
            let kind = if EXCLUDED_SEGMENTS.contains(&name) {
                EntryKind::Other
            } else {
                let kind = std::fs::symlink_metadata(&path)
                    .map_err(|e| io(rel.clone(), e))?
                    .file_type();
                if kind.is_symlink() {
                    EntryKind::Symlink
                } else if kind.is_dir() {
                    EntryKind::Dir
                } else if kind.is_file() {
                    EntryKind::File(std::fs::read(&path).map_err(|e| io(rel, e))?)
                } else {
                    EntryKind::Other
                }
            };
            if pack.admit(&segments, kind)? {
                walk(&path, &segments, pack)?;
            }
        }
        Ok(())
    }
    let mut pack = Collector::default();
    walk(root, &[], &mut pack)?;
    pack.finish()
}

/// Read a gzipped tar as a pack. An absolute or escaping entry is refused; `./` prefixes are
/// dropped. An archive that gunzips past [`MAX_EXPANDED_BYTES`] is refused.
pub fn read_tar_gz(tgz: &[u8]) -> Result<ReadPack, TreeError> {
    read_tar_gz_within(tgz, MAX_EXPANDED_BYTES)
}

/// A reader that fails once its inner reader yields more than `remaining` bytes.
struct Bounded<R> {
    inner: R,
    remaining: u64,
    exceeded: bool,
}

impl<R: std::io::Read> std::io::Read for Bounded<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let cap = usize::try_from(self.remaining.saturating_add(1))
            .map_or(buf.len(), |cap| cap.min(buf.len()));
        let n = self.inner.read(&mut buf[..cap])?;
        match self.remaining.checked_sub(n as u64) {
            Some(left) => {
                self.remaining = left;
                Ok(n)
            }
            None => {
                self.exceeded = true;
                Err(std::io::Error::other("the pack archive is too large"))
            }
        }
    }
}

fn read_tar_gz_within(tgz: &[u8], limit: u64) -> Result<ReadPack, TreeError> {
    let mut archive = tar::Archive::new(Bounded {
        inner: flate2::read::GzDecoder::new(tgz),
        remaining: limit,
        exceeded: false,
    });
    let read = collect_archive(&mut archive);
    if archive.into_inner().exceeded {
        return Err(TreeError::Expands { limit });
    }
    read
}

fn collect_archive<R: std::io::Read>(archive: &mut tar::Archive<R>) -> Result<ReadPack, TreeError> {
    let archive_err = |message: String| TreeError::Io {
        path: "the pack archive".to_string(),
        message,
    };
    let mut pack = Collector::default();
    for entry in archive.entries().map_err(|e| archive_err(e.to_string()))? {
        let mut entry = entry.map_err(|e| archive_err(e.to_string()))?;
        let raw = entry
            .path()
            .map_err(|e| archive_err(e.to_string()))?
            .into_owned();
        let shown = raw.to_string_lossy().into_owned();
        let mut segments = Vec::new();
        for component in raw.components() {
            match component {
                Component::Normal(seg) => {
                    segments.push(seg.to_str().ok_or_else(|| TreeError::InvalidPath {
                        path: shown.clone(),
                        reason: "is not valid UTF-8".to_string(),
                    })?)
                }
                Component::CurDir => {}
                _ => {
                    return Err(TreeError::InvalidPath {
                        path: shown,
                        reason: "escapes the pack root".to_string(),
                    });
                }
            }
        }
        let kind = match entry.header().entry_type() {
            tar::EntryType::Directory => EntryKind::Dir,
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut bytes)
                    .map_err(|e| archive_err(format!("{shown}: {e}")))?;
                EntryKind::File(bytes)
            }
            tar::EntryType::Symlink => EntryKind::Symlink,
            _ => EntryKind::Other,
        };
        pack.admit(&segments, kind)?;
    }
    pack.finish()
}

#[cfg(test)]
mod tests {
    use crate::pack_tree::*;

    fn tree(files: &[(&str, &[u8])]) -> PackTree {
        PackTree::from_pairs(files).expect("tree")
    }

    /// Path, type, mode, mtime, bytes, and link target of one hand-built tar entry.
    type RawEntry<'a> = (&'a str, tar::EntryType, u32, u64, &'a [u8], Option<&'a str>);

    /// Build a tar.gz entry by entry, so a test controls every header field.
    fn raw_tgz(level: u32, entries: &[RawEntry<'_>]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(level));
        let mut builder = tar::Builder::new(gz);
        for (path, kind, mode, mtime, bytes, link) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_mode(*mode);
            header.set_mtime(*mtime);
            header.set_size(bytes.len() as u64);
            match link {
                Some(target) => builder
                    .append_link(&mut header, path, target)
                    .expect("link"),
                None => builder
                    .append_data(&mut header, path, *bytes)
                    .expect("append"),
            }
        }
        builder.into_inner().expect("tar").finish().expect("gzip")
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
            let err = bad.parse::<PackFilePath>().expect_err(bad).to_string();
            assert!(err.contains(why), "{bad}: {err}");
        }
        assert!("tools/measure.sh".parse::<PackFilePath>().is_ok());
    }

    #[test]
    fn a_file_another_path_needs_as_a_directory_is_refused() {
        let files: BTreeMap<PackFilePath, Vec<u8>> = [
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
        let files: BTreeMap<PackFilePath, Vec<u8>> = [("x/state/y", b"1".to_vec())]
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

        let read = walk_dir(&root).expect("walk");

        let paths: Vec<&str> = read.tree.files().keys().map(PackFilePath::as_str).collect();
        assert_eq!(paths, vec!["crucible.toml", "tools/run.sh"]);
        assert_eq!(
            read.ignored,
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

    #[test]
    fn a_tree_round_trips_through_its_tarball_byte_for_byte() {
        let pack = tree(&[("crucible.toml", b"m"), ("tools/run.sh", b"#!/bin/sh\n")]);
        let tarball = pack.tarball().expect("tarball");
        assert_eq!(pack.tarball().expect("again"), tarball);
        let read = read_tar_gz(&tarball).expect("read");
        assert_eq!(read.tree, pack);
        assert!(read.ignored.is_empty());
    }

    #[test]
    fn archives_of_the_same_files_name_one_pack() {
        use tar::EntryType::{Directory, Regular};
        let a = raw_tgz(
            1,
            &[
                ("tools", Directory, 0o755, 1, b"", None),
                ("tools/run.sh", Regular, 0o644, 1, b"x", None),
                ("crucible.toml", Regular, 0o600, 2, b"m", None),
            ],
        );
        let b = raw_tgz(
            9,
            &[
                ("./crucible.toml", Regular, 0o755, 999, b"m", None),
                ("./tools/run.sh", Regular, 0o755, 999, b"x", None),
            ],
        );
        let expected = tree(&[("crucible.toml", b"m"), ("tools/run.sh", b"x")]);
        assert_eq!(read_tar_gz(&a).expect("a").tree, expected);
        assert_eq!(read_tar_gz(&b).expect("b").tree, expected);
    }

    #[test]
    fn excluded_archive_entries_are_dropped_and_reported_whatever_their_type() {
        use tar::EntryType::{Regular, Symlink};
        let tgz = raw_tgz(
            6,
            &[
                ("crucible.toml", Regular, 0o644, 0, b"m", None),
                ("state/session.jsonl", Regular, 0o644, 0, b"s", None),
                ("workspace/repo/a", Regular, 0o644, 0, b"w", None),
                ("x/.git/link", Symlink, 0o777, 0, b"", Some("../y")),
            ],
        );
        let read = read_tar_gz(&tgz).expect("read");
        assert_eq!(read.tree, tree(&[("crucible.toml", b"m")]));
        assert_eq!(read.ignored, vec!["state", "workspace", "x/.git"]);
    }

    #[test]
    fn an_archive_no_pack_may_be_is_refused_naming_the_entry() {
        use tar::EntryType::{Link, Regular, Symlink};
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (
                raw_tgz(
                    6,
                    &[("link", Symlink, 0o777, 0, b"", Some("crucible.toml"))],
                ),
                "link",
            ),
            (
                raw_tgz(
                    6,
                    &[
                        ("a", Regular, 0o644, 0, b"x", None),
                        ("hard", Link, 0o644, 0, b"", Some("a")),
                    ],
                ),
                "hard",
            ),
            (
                raw_tgz(
                    6,
                    &[
                        ("a", Regular, 0o644, 0, b"x", None),
                        ("a/b", Regular, 0o644, 0, b"y", None),
                    ],
                ),
                "a/b",
            ),
        ];
        for (tgz, named) in cases {
            let err = read_tar_gz(&tgz).expect_err(named).to_string();
            assert!(err.contains(named), "{named}: {err}");
        }
    }

    #[test]
    fn an_escaping_archive_entry_is_refused() {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(1);
        header.as_gnu_mut().expect("gnu").name[..7].copy_from_slice(b"../evil");
        header.set_cksum();
        builder.append(&header, &b"x"[..]).expect("append");
        let tgz = builder.into_inner().expect("tar").finish().expect("gzip");

        let err = read_tar_gz(&tgz).expect_err("escapes").to_string();
        assert!(err.contains("escapes"), "{err}");
    }

    #[test]
    fn an_archive_that_expands_past_the_limit_is_refused() {
        let pack = tree(&[("big", &[0u8; 4096])]);
        let tarball = pack.tarball().expect("tarball");

        let mut expanded = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::GzDecoder::new(&tarball[..]),
            &mut expanded,
        )
        .expect("gunzip");
        assert!(
            read_tar_gz_within(&tarball, expanded.len() as u64)
                .expect("the whole stream fits")
                .tree
                == pack
        );
        for limit in [4096, 600, 0] {
            assert!(
                read_tar_gz_within(&tarball, limit) == Err(TreeError::Expands { limit }),
                "limit {limit}"
            );
        }
        let bomb = tree(&[("zeros", &vec![0u8; 8 * 1024 * 1024])])
            .tarball()
            .expect("tarball");
        assert!(bomb.len() < 64 * 1024, "{} gzipped bytes", bomb.len());
        assert!(
            read_tar_gz_within(&bomb, 1024 * 1024)
                == Err(TreeError::Expands { limit: 1024 * 1024 })
        );
    }

    #[test]
    fn the_delivery_budget_refuses_only_a_larger_tarball() {
        assert_eq!(
            check_delivery_budget(&vec![0; DELIVERY_BUDGET_BYTES]),
            Ok(())
        );
        let over = check_delivery_budget(&vec![0; DELIVERY_BUDGET_BYTES + 1]).expect_err("over");
        assert_eq!(over.bytes, DELIVERY_BUDGET_BYTES + 1);
        assert!(over.to_string().contains("delivery budget"), "{over}");
    }

    #[test]
    fn the_file_hashes_are_the_ones_the_digest_hashes() {
        let pack = tree(&[("a", b"hello\n")]);
        let (digest, hashes) = pack.digest_with_file_hashes();
        assert_eq!(digest, pack.digest());
        assert_eq!(
            hashes,
            vec!["5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03".to_string()]
        );
    }
}
