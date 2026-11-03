#![allow(clippy::disallowed_macros)]

use std::fmt;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// gzipped for streaming over exec stdin (the interactive path)
pub fn tarball(ctx: &Path) -> Result<Vec<u8>> {
    gzip(&tar_bytes(ctx)?)
}

pub fn gzip(tar: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(tar).context("gzipping tar")?;
    enc.finish().context("finalizing gzip")
}

/// The digest of a build context as buildit uploads it, `sha256:<hex>`.
///
/// It hashes one record per archive entry, sorted by path:
///
/// ```text
/// directory:  "D" NUL path NUL
/// file:       "F" NUL path NUL mode NUL sha256-hex(content) NUL
/// symlink:    "L" NUL path NUL target NUL
/// ```
///
/// `mode` is the file's permission bits as four octal digits. Owners, times, directory and
/// symlink modes, and archive order do not count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContextDigest(String);

impl ContextDigest {
    pub fn parse(raw: &str) -> Result<Self> {
        let ok = raw.strip_prefix("sha256:").is_some_and(|hex| {
            hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        });
        if !ok {
            bail!("{raw:?} is not a sha256:<hex> context digest");
        }
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ContextDigest {
    type Error = anyhow::Error;

    fn try_from(raw: String) -> Result<Self> {
        Self::parse(&raw)
    }
}

impl From<ContextDigest> for String {
    fn from(d: ContextDigest) -> Self {
        d.0
    }
}

impl fmt::Display for ContextDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn content_sha256(content: &mut impl Read) -> Result<Vec<u8>> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = content
            .read(&mut buf)
            .context("reading a context file's bytes")?;
        if n == 0 {
            return Ok(hex(&hasher.finalize()).into_bytes());
        }
        hasher.update(&buf[..n]);
    }
}

/// The [`ContextDigest`] of an uncompressed context tar.
pub fn digest(tar: &[u8]) -> Result<ContextDigest> {
    use tar::EntryType;
    let mut records: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for entry in tar::Archive::new(tar)
        .entries()
        .context("reading the context tar")?
    {
        let mut entry = entry.context("reading a context tar entry")?;
        let raw = entry.path_bytes().into_owned();
        let path = raw.strip_suffix(b"/").unwrap_or(&raw).to_vec();
        let mode = entry.header().mode().context("reading an entry's mode")? & 0o7777;
        let mode = format!("{mode:04o}").into_bytes();
        let fields = match entry.header().entry_type() {
            EntryType::Directory => vec![b"D".to_vec(), path.clone()],
            EntryType::Regular | EntryType::Continuous => {
                vec![
                    b"F".to_vec(),
                    path.clone(),
                    mode,
                    content_sha256(&mut entry)?,
                ]
            }
            EntryType::Symlink => {
                let target = entry
                    .link_name_bytes()
                    .ok_or_else(|| anyhow!("symlink {} has no target", path.escape_ascii()))?
                    .into_owned();
                vec![b"L".to_vec(), path.clone(), target]
            }
            other => bail!(
                "the context holds {} as {other:?}, which buildit never archives",
                path.escape_ascii()
            ),
        };
        let record = fields
            .into_iter()
            .flat_map(|f| f.into_iter().chain([0]))
            .collect();
        records.push((path, record));
    }
    records.sort();
    let mut hasher = Sha256::new();
    for (_, record) in &records {
        hasher.update(record);
    }
    Ok(ContextDigest(format!("sha256:{}", hex(&hasher.finalize()))))
}

/// The digest `dir` gets as a build context from a local workspace: staged, sanitized and
/// archived as a build stages, sanitizes and archives it.
pub fn digest_dir(dir: &Path) -> Result<ContextDigest> {
    let stage = tempfile::tempdir().context("creating staging dir")?;
    let ctx = stage.path().join("ctx");
    crate::sandbox::stage_local(dir, &ctx)?;
    digest(&tar_bytes(&ctx)?)
}

// honors .dockerignore; .git/ and target/ always skipped because nobody
// wants to ship a mac-arch target dir to an amd64 builder. uncompressed so
// detach mode can use one sha256 as both blob digest and diff_id.
pub fn tar_bytes(ctx: &Path) -> Result<Vec<u8>> {
    if !ctx.is_dir() {
        return Err(anyhow!("context {} is not a directory", ctx.display()));
    }
    let mut walk = WalkBuilder::new(ctx);
    walk.standard_filters(false)
        .hidden(false)
        .add_custom_ignore_filename(".dockerignore");
    walk.filter_entry(|entry| {
        entry.depth() != 1 || (entry.file_name() != ".git" && entry.file_name() != "target")
    });

    let mut tar = tar::Builder::new(Vec::new());
    tar.follow_symlinks(false);

    for entry in walk.build() {
        let entry = entry.context("walking build context")?;
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(ctx)
            .with_context(|| format!("relativizing {}", path.display()))?;
        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
        if is_dir {
            tar.append_dir(rel, path)
                .with_context(|| format!("archiving dir {}", rel.display()))?;
        } else {
            tar.append_path_with_name(path, rel)
                .with_context(|| format!("archiving {}", rel.display()))?;
        }
    }

    tar.into_inner().context("finalizing tar")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::time::{Duration, SystemTime};

    use flate2::read::GzDecoder;

    use crate::context::{ContextDigest, digest, digest_dir, tar_bytes, tarball};
    use crate::sandbox::{RelPath, SandboxName, Workspace};

    #[derive(Clone, Copy)]
    enum Item {
        Dir(&'static str),
        File(&'static str, u32, &'static [u8]),
        Link(&'static str, &'static str),
    }

    struct Meta {
        mtime: u64,
        owner: u64,
        dir_mode: u32,
        link_mode: u32,
    }

    const META: Meta = Meta {
        mtime: 1_700_000_000,
        owner: 1000,
        dir_mode: 0o755,
        link_mode: 0o777,
    };

    fn tar_of(items: &[Item], meta: &Meta) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for item in items {
            let mut h = tar::Header::new_gnu();
            h.set_mtime(meta.mtime);
            h.set_uid(meta.owner);
            h.set_gid(meta.owner);
            h.set_size(0);
            match *item {
                Item::Dir(path) => {
                    h.set_entry_type(tar::EntryType::Directory);
                    h.set_mode(meta.dir_mode);
                    tar.append_data(&mut h, path, &[][..]).unwrap();
                }
                Item::File(path, mode, body) => {
                    h.set_entry_type(tar::EntryType::Regular);
                    h.set_mode(mode);
                    h.set_size(body.len() as u64);
                    tar.append_data(&mut h, path, body).unwrap();
                }
                Item::Link(path, target) => {
                    h.set_entry_type(tar::EntryType::Symlink);
                    h.set_mode(meta.link_mode);
                    tar.append_link(&mut h, path, target).unwrap();
                }
            }
        }
        tar.into_inner().unwrap()
    }

    const SVC: [Item; 4] = [
        Item::Dir("svc"),
        Item::File("svc/Dockerfile", 0o644, b"FROM scratch\n"),
        Item::Link("svc/link", "Dockerfile"),
        Item::File("svc/run.sh", 0o755, b"#!/bin/sh\n"),
    ];

    #[test]
    fn the_digest_is_the_documented_record_hash() {
        // computed apart from this code, from the record format on ContextDigest
        assert_eq!(
            digest(&tar_of(&SVC, &META)).unwrap().as_str(),
            "sha256:0fd63955ea266fe498f9a5cf0f3046bc3c8f7ed56ef968b6df82de2d79ae6092"
        );
    }

    #[test]
    fn order_times_owners_and_directory_and_link_modes_do_not_count() {
        let base = digest(&tar_of(&SVC, &META)).unwrap();
        let mut reversed = SVC;
        reversed.reverse();
        let other = Meta {
            mtime: 42,
            owner: 0,
            dir_mode: 0o700,
            link_mode: 0o755,
        };
        assert_eq!(digest(&tar_of(&reversed, &META)).unwrap(), base);
        assert_eq!(digest(&tar_of(&SVC, &other)).unwrap(), base);
        assert_eq!(digest(&tar_of(&reversed, &other)).unwrap(), base);
    }

    #[test]
    fn content_file_modes_link_targets_kinds_and_paths_each_count() {
        let variants: [&[Item]; 8] = [
            &SVC,
            &[
                Item::Dir("svc"),
                Item::File("svc/Dockerfile", 0o644, b"FROM scratch \n"),
                Item::Link("svc/link", "Dockerfile"),
                Item::File("svc/run.sh", 0o755, b"#!/bin/sh\n"),
            ],
            &[
                Item::Dir("svc"),
                Item::File("svc/Dockerfile", 0o644, b"FROM scratch\n"),
                Item::Link("svc/link", "Dockerfile"),
                Item::File("svc/run.sh", 0o644, b"#!/bin/sh\n"),
            ],
            &[
                Item::Dir("svc"),
                Item::File("svc/Dockerfile", 0o644, b"FROM scratch\n"),
                Item::Link("svc/link", "run.sh"),
                Item::File("svc/run.sh", 0o755, b"#!/bin/sh\n"),
            ],
            &[
                Item::Dir("svc"),
                Item::File("svc/Dockerfile", 0o644, b"FROM scratch\n"),
                Item::File("svc/link", 0o644, b"Dockerfile"),
                Item::File("svc/run.sh", 0o755, b"#!/bin/sh\n"),
            ],
            &[
                Item::Dir("svc"),
                Item::File("svc/Dockerfile", 0o644, b"FROM scratch\n"),
                Item::Link("svc/link", "Dockerfile"),
                Item::File("svc/start.sh", 0o755, b"#!/bin/sh\n"),
            ],
            &[
                Item::File("svc/Dockerfile", 0o644, b"FROM scratch\n"),
                Item::Link("svc/link", "Dockerfile"),
                Item::File("svc/run.sh", 0o755, b"#!/bin/sh\n"),
            ],
            &[
                Item::Dir("svc"),
                Item::Dir("svc/empty"),
                Item::File("svc/Dockerfile", 0o644, b"FROM scratch\n"),
                Item::Link("svc/link", "Dockerfile"),
                Item::File("svc/run.sh", 0o755, b"#!/bin/sh\n"),
            ],
        ];
        let digests: BTreeSet<String> = variants
            .iter()
            .map(|items| digest(&tar_of(items, &META)).unwrap().to_string())
            .collect();
        assert_eq!(digests.len(), variants.len(), "{digests:?}");
    }

    #[test]
    fn a_field_cannot_slide_into_its_neighbour() {
        let a = [Item::File("ab", 0o644, b"")];
        let b = [Item::File("a", 0o644, b"b")];
        let c = [Item::Link("a", "b"), Item::Dir("c")];
        let d = [Item::Link("a", "bc")];
        assert_ne!(
            digest(&tar_of(&a, &META)).unwrap(),
            digest(&tar_of(&b, &META)).unwrap()
        );
        assert_ne!(
            digest(&tar_of(&c, &META)).unwrap(),
            digest(&tar_of(&d, &META)).unwrap()
        );
    }

    #[test]
    fn special_entries_are_refused() {
        let mut tar = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Fifo);
        h.set_mode(0o644);
        h.set_size(0);
        tar.append_data(&mut h, "pipe", &[][..]).unwrap();
        let e = digest(&tar.into_inner().unwrap()).unwrap_err();
        assert!(format!("{e:#}").contains("never archives"), "{e:#}");
    }

    #[test]
    fn digests_parse_only_as_sha256_hex() {
        let good = format!("sha256:{}", "0a".repeat(32));
        assert_eq!(ContextDigest::parse(&good).unwrap().as_str(), good);
        for bad in [
            "",
            "sha256:",
            &format!("sha512:{}", "0a".repeat(32)),
            &format!("sha256:{}", "0A".repeat(32)),
            &format!("sha256:{}", "0a".repeat(31)),
            &format!("sha256:{}0", "0a".repeat(32)),
        ] {
            assert!(ContextDigest::parse(bad).is_err(), "{bad:?}");
        }
        let json = serde_json::json!(good);
        let back: ContextDigest = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(&back).unwrap(), json);
        assert!(serde_json::from_value::<ContextDigest>(serde_json::json!("sha256:zz")).is_err());
    }

    // a tree with a file, an executable, a nested dir, an inner and an outer link, an excluded
    // dir and a dockerignored file; `stamp` sets every file's mtime
    fn tree(root: &Path, stamp: SystemTime, reverse: bool) {
        let mut files: Vec<(&str, &[u8], u32)> = vec![
            ("svc/Dockerfile", b"FROM scratch\nCOPY . /\n", 0o644),
            ("svc/bin/run.sh", b"#!/bin/sh\necho hi\n", 0o755),
            ("svc/.git/HEAD", b"ref: x\n", 0o644),
            ("svc/notes.txt", b"ignored\n", 0o644),
            ("svc/.dockerignore", b"notes.txt\n", 0o644),
            ("secret", b"outside\n", 0o600),
        ];
        if reverse {
            files.reverse();
        }
        for (rel, body, mode) in files {
            let path = root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(stamp)
                .unwrap();
        }
        symlink("Dockerfile", root.join("svc/inner")).unwrap();
        symlink("../secret", root.join("svc/outer")).unwrap();
    }

    #[test]
    fn a_directory_digests_as_a_local_build_uploads_it_whenever_it_was_written() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        tree(first.path(), SystemTime::UNIX_EPOCH, false);
        tree(
            second.path(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
            true,
        );
        let ours = digest_dir(&first.path().join("svc")).unwrap();
        assert_eq!(digest_dir(&second.path().join("svc")).unwrap(), ours);

        let stage = tempfile::tempdir().unwrap();
        let ctx = stage.path().join("ctx");
        let workspace = Workspace::Local {
            dir: second.path().to_path_buf(),
        };
        workspace
            .fetch_context(
                &SandboxName::parse("loop").unwrap(),
                &RelPath::parse("svc").unwrap(),
                &ctx,
            )
            .unwrap();
        let uploaded = tar_bytes(&ctx).unwrap();
        assert_eq!(digest(&uploaded).unwrap(), ours, "the build's own staging");
        let names = entries(&crate::context::gzip(&uploaded).unwrap());
        for kept in ["Dockerfile", "bin/run.sh", "inner", ".dockerignore"] {
            assert!(names.contains(kept), "{kept}: {names:?}");
        }
        for dropped in ["outer", ".git/HEAD", "notes.txt"] {
            assert!(!names.contains(dropped), "{dropped}: {names:?}");
        }

        let script = first.path().join("svc/bin/run.sh");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
        assert_ne!(digest_dir(&first.path().join("svc")).unwrap(), ours);
    }

    fn entries(data: &[u8]) -> BTreeSet<String> {
        let mut archive = tar::Archive::new(GzDecoder::new(data));
        archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    #[test]
    fn skips_git_target_and_dockerignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("Dockerfile"));
        touch(&root.join("src/main.rs"));
        touch(&root.join(".git/HEAD"));
        touch(&root.join("target/debug/foo"));
        touch(&root.join("secrets.env"));
        // nested dirs named target are NOT the cargo dir, keep them
        touch(&root.join("src/target/keep.rs"));
        fs::write(root.join(".dockerignore"), "secrets.env\n").unwrap();

        let names = entries(&tarball(root).unwrap());
        assert!(names.contains("Dockerfile"), "{names:?}");
        assert!(names.contains("src/main.rs"), "{names:?}");
        assert!(names.contains("src/target/keep.rs"), "{names:?}");
        assert!(!names.iter().any(|n| n.starts_with(".git")), "{names:?}");
        assert!(!names.iter().any(|n| n.starts_with("target")), "{names:?}");
        assert!(!names.contains("secrets.env"), "{names:?}");
    }

    #[test]
    fn rejects_non_directory_context() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        touch(&file);
        assert!(tarball(&file).is_err());
    }
}
