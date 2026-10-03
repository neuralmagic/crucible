//! Pack storage over Postgres: the durable form of a frozen scope pack is the gzipped tarball in
//! `pack_tarballs` (keyed by the sanitized issue key), and human steering lives beside it as
//! `pack_steering` rows. Readers that need a working tree — the engine's PR push, `crucible
//! deploy render`, `[build]` planning — call [`materialize_pack`], which unpacks the tarball into
//! a scratch [`tempfile::TempDir`] and injects the steering rows onto `STEER.md`, in the exact
//! marker-wrapped shape `crucible/src/control.rs::append_steer` writes. Readers that need one
//! file (`SCOPE.md`) scan the tarball in memory via [`read_pack_file`].

#![allow(clippy::disallowed_macros)]

use anyhow::{Context, Result, bail};
use crucible_contract::pack_tree::{EXCLUDED_SEGMENTS, PackPath, PackTree, TreeError, walk_dir};
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A pack tree unpacked to scratch. The tempdir guard lives inside, so the tree exists exactly as
/// long as this value — keep it alive for the duration of any subprocess reading [`Self::path`].
pub struct MaterializedPack {
    _guard: tempfile::TempDir,
    root: PathBuf,
}

impl MaterializedPack {
    pub fn path(&self) -> &Path {
        &self.root
    }
}

/// Why the pack tree at `root` is over the run delivery budget, or `None` when it fits: its
/// delivered tarball (the engine's [`crucible::deploy::pack_delivery_tarball`]) against
/// [`crucible::deploy::PACK_DELIVERY_BUDGET_BYTES`].
pub(crate) fn over_delivery_budget(root: &Path) -> Result<Option<String>> {
    let budget = crucible::deploy::PACK_DELIVERY_BUDGET_BYTES;
    let bytes = crucible::deploy::pack_delivery_tarball(root)
        .context("sizing the pack's delivered tarball")?
        .len();
    Ok((bytes > budget).then(|| {
        format!("the pack delivers {bytes} gzipped bytes to a run, over the {budget}-byte delivery budget")
    }))
}

/// Gzip-tar a pack working tree (regular files + symlinks, relative paths, `.git` excluded — a
/// pack is authored files, never a repo; the PR push `git init`s its own scratch copy).
pub(crate) fn tar_pack_tree(tree: &Path) -> Result<Vec<u8>> {
    let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);
    append_dir(&mut builder, tree, Path::new(""))?;
    let enc = builder.into_inner().context("finishing the pack tar")?;
    enc.finish().context("gzipping the pack tar")
}

fn append_dir(
    builder: &mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>,
    dir: &Path,
    rel: &Path,
) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading pack dir {}", dir.display()))?
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("reading pack dir {}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        if rel.as_os_str().is_empty() && name == ".git" {
            continue;
        }
        let path = entry.path();
        let entry_rel = rel.join(&name);
        let ft = entry
            .file_type()
            .with_context(|| format!("stat {}", path.display()))?;
        if ft.is_dir() {
            append_dir(builder, &path, &entry_rel)?;
        } else {
            builder
                .append_path_with_name(&path, &entry_rel)
                .with_context(|| format!("taring pack entry {}", entry_rel.display()))?;
        }
    }
    Ok(())
}

/// Store a pod-delivered pack tarball for `key`, validating it first (a scratch unpack runs the
/// same traversal rejection every materialization does, so a hostile blob is refused before it
/// becomes the durable pack). Returns the stored digest.
#[cfg(feature = "autoresearch")]
pub(crate) async fn store_pack_tarball(pool: &PgPool, key: &str, tar_gz: &[u8]) -> Result<String> {
    let scratch = tempfile::tempdir().context("pack validation scratch dir")?;
    unpack_pack_tgz(tar_gz, &scratch.path().join("pack")).context("validating the pack tarball")?;
    crate::runs::blob_store::put_pack_tarball(pool, &crate::model::sanitize_key(key), tar_gz).await
}

/// Tar a locally-written pack tree and store it as `key`'s durable tarball.
pub async fn store_pack_tree(pool: &PgPool, key: &str, tree: &Path) -> Result<String> {
    let tar_gz = tar_pack_tree(tree)?;
    crate::runs::blob_store::put_pack_tarball(pool, &crate::model::sanitize_key(key), &tar_gz).await
}

/// Unpack a stored tarball into a scratch tree, through the same traversal rejection every
/// materialization runs. The tempdir guard rides the returned value.
pub(crate) fn unpack_to_scratch(tar_gz: &[u8]) -> Result<MaterializedPack> {
    let guard = tempfile::tempdir().context("pack materialization scratch dir")?;
    let root = guard.path().join("pack");
    unpack_pack_tgz(tar_gz, &root)?;
    Ok(MaterializedPack {
        _guard: guard,
        root,
    })
}

/// Unpack `key`'s stored tarball into a scratch tree and append its steering rows onto `STEER.md`
/// (ordered by seq, each stamped with the row's own recorded time — so the same tarball and rows
/// always materialize to the same bytes). `None` when no pack was ever stored.
pub(crate) async fn materialize_pack(pool: &PgPool, key: &str) -> Result<Option<MaterializedPack>> {
    let slug = crate::model::sanitize_key(key);
    let Some(tar_gz) = crate::runs::blob_store::get_pack_tarball(pool, &slug).await? else {
        return Ok(None);
    };
    let MaterializedPack {
        _guard: guard,
        root,
    } = unpack_to_scratch(&tar_gz)
        .with_context(|| format!("unpacking the stored pack for {key}"))?;
    let steering = crate::runs::blob_store::list_steering(pool, &slug).await?;
    if !steering.is_empty() {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("STEER.md"))
            .context("opening STEER.md for steering injection")?;
        for s in &steering {
            let ts: jiff::Timestamp = s
                .created_at
                .parse()
                .with_context(|| format!("parsing steering timestamp {:?}", s.created_at))?;
            let payload = format!(
                "<!-- steer @{} by control -->\n{}\n",
                ts.as_second(),
                s.body_md.trim()
            );
            f.write_all(payload.as_bytes())
                .context("appending steering to STEER.md")?;
        }
    }
    Ok(Some(MaterializedPack {
        _guard: guard,
        root,
    }))
}

/// [`materialize_pack`], falling back to an empty scratch tree when no pack is stored — the
/// pre-tarball semantics of a missing `packs/<key>/` dir (`plan_builds` sees no manifest; a real
/// `deploy render` fails loudly at dispatch).
#[cfg(feature = "autoresearch")]
pub(crate) async fn materialize_pack_or_empty(
    pool: &PgPool,
    key: &str,
) -> Result<MaterializedPack> {
    if let Some(pack) = materialize_pack(pool, key).await? {
        return Ok(pack);
    }
    let guard = tempfile::tempdir().context("pack materialization scratch dir")?;
    let root = guard.path().join("pack");
    std::fs::create_dir_all(&root).context("creating the empty pack tree")?;
    Ok(MaterializedPack {
        _guard: guard,
        root,
    })
}

/// Read one file (by pack-relative path) straight out of `key`'s stored tarball, no disk touch.
/// `None` when no pack is stored or the file isn't in it.
#[cfg(feature = "autoresearch")]
pub(crate) async fn read_pack_file(pool: &PgPool, key: &str, name: &str) -> Result<Option<String>> {
    use std::io::Read as _;
    let slug = crate::model::sanitize_key(key);
    let Some(tar_gz) = crate::runs::blob_store::get_pack_tarball(pool, &slug).await? else {
        return Ok(None);
    };
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tar_gz.as_slice()));
    for entry in archive.entries().context("reading the pack tar")? {
        let mut entry = entry.context("reading a pack tar entry")?;
        let path = entry.path().context("decoding a pack tar entry path")?;
        let rel = path.strip_prefix(".").unwrap_or(&path);
        if rel == Path::new(name) {
            let mut s = String::new();
            entry
                .read_to_string(&mut s)
                .with_context(|| format!("reading {name} from the stored pack for {key}"))?;
            return Ok(Some(s));
        }
    }
    Ok(None)
}

/// Why a pack tree could not be read back as text files.
#[derive(Debug, thiserror::Error)]
pub enum ReadTreeError {
    #[error("{0} is not text")]
    NotText(String),
    #[error(transparent)]
    NotAPack(#[from] TreeError),
}

/// Read a pack tree back as a `{path: content}` map. A non-UTF-8 file is refused rather than
/// dropped: an editor that round-trips the whole map on every save would delete a file it
/// cannot show.
pub(crate) fn read_tree(root: &Path) -> Result<BTreeMap<String, String>, ReadTreeError> {
    walk_dir(root)?
        .tree
        .into_files()
        .into_iter()
        .map(|(path, bytes)| {
            let path = String::from(path);
            String::from_utf8(bytes)
                .map(|text| (path.clone(), text))
                .map_err(|_| ReadTreeError::NotText(path))
        })
        .collect()
}

/// A stored archive read as a pack: the tree, and the excluded paths it held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ingested {
    pub tree: PackTree,
    pub ignored: Vec<String>,
}

/// Read a gzipped tar as a pack under RFC-0002:C-PACK-BASE. Entries under an excluded segment are
/// dropped and reported, whatever their type. Outside them a symlink, hard link, or special file,
/// an absolute or escaping path, or a path no pack may hold is refused, naming the entry. Modes
/// and timestamps are dropped; a later entry for the same path replaces an earlier one, as `tar`
/// extraction does.
pub(crate) fn tree_from_tar_gz(tgz: &[u8]) -> Result<Ingested, TreeError> {
    let archive_err = |message: String| TreeError::Io {
        path: "the pack archive".to_string(),
        message,
    };
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tgz));
    let mut files: BTreeMap<PackPath, Vec<u8>> = BTreeMap::new();
    let mut ignored: Vec<String> = Vec::new();
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
                std::path::Component::Normal(seg) => {
                    segments.push(seg.to_str().ok_or_else(|| TreeError::InvalidPath {
                        path: shown.clone(),
                        reason: "is not valid UTF-8".to_string(),
                    })?)
                }
                std::path::Component::CurDir => {}
                _ => {
                    return Err(TreeError::InvalidPath {
                        path: shown,
                        reason: "escapes the pack root".to_string(),
                    });
                }
            }
        }
        if segments.is_empty() {
            continue;
        }
        if let Some(i) = segments.iter().position(|s| EXCLUDED_SEGMENTS.contains(s)) {
            let excluded = segments[..=i].join("/");
            if !ignored.contains(&excluded) {
                ignored.push(excluded);
            }
            continue;
        }
        let rel = segments.join("/");
        match entry.header().entry_type() {
            tar::EntryType::Directory => {}
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let path: PackPath = rel.parse()?;
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut bytes)
                    .map_err(|e| archive_err(format!("{rel}: {e}")))?;
                files.insert(path, bytes);
            }
            tar::EntryType::Symlink => return Err(TreeError::Symlink { path: rel }),
            _ => return Err(TreeError::NonRegular { path: rel }),
        }
    }
    ignored.sort();
    Ok(Ingested {
        tree: PackTree::new(files)?,
        ignored,
    })
}

/// The gzipped tar a legacy pack-byte column holds for `tree`, so a controller that predates
/// tree storage reads the same files.
pub(crate) fn encode_legacy_tar_gz(tree: &PackTree) -> Result<Vec<u8>> {
    crucible::deploy::encode_pack_tarball(
        tree.files()
            .iter()
            .map(|(path, bytes)| (path.as_str(), bytes.as_slice())),
    )
}

/// Unpack a scope pack blob (gzip'd tar) into `dest`, replacing whatever was there — a stale pack
/// from a prior scope must never mix with the incoming one. Every entry path is validated before
/// it touches the filesystem: absolute paths and `..` components reject the whole pack (defense in
/// depth on top of tar's own `unpack_in` guard) — the blob came out of an agent-authored pod.
pub(crate) fn unpack_pack_tgz(tgz: &[u8], dest: &Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)
            .with_context(|| format!("clearing the stale pack dir {}", dest.display()))?;
    }
    std::fs::create_dir_all(dest)
        .with_context(|| format!("creating the pack dir {}", dest.display()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tgz));
    for entry in archive.entries().context("reading the pack tar")? {
        let mut entry = entry.context("reading a pack tar entry")?;
        let path = entry.path().context("decoding a pack tar entry path")?;
        let escapes = path.is_absolute()
            || path.components().any(|c| {
                !matches!(
                    c,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            });
        if escapes {
            bail!(
                "pack tar entry {:?} escapes the pack dir; rejecting the whole pack",
                path
            );
        }
        let path = path.into_owned();
        if !entry
            .unpack_in(dest)
            .with_context(|| format!("unpacking pack tar entry {path:?}"))?
        {
            bail!("pack tar entry {path:?} was refused by the unpacker; rejecting the whole pack");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("crucible.toml"), "[repo]\nurl = \"x\"\n").unwrap();
        std::fs::write(dir.path().join("SCOPE.md"), "identity: v1:beef\n").unwrap();
        std::fs::create_dir_all(dir.path().join("gates")).unwrap();
        std::fs::write(dir.path().join("gates").join("judge.py"), "print(1)\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git").join("HEAD"), "ref: nope\n").unwrap();
        dir
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn tree_roundtrips_through_store_and_materialize(pool: sqlx::PgPool) {
        let tree = sample_tree();
        store_pack_tree(&pool, "owner/repo#7", tree.path())
            .await
            .expect("store");
        let pack = materialize_pack(&pool, "owner/repo#7")
            .await
            .expect("materialize")
            .expect("stored");
        assert_eq!(
            std::fs::read_to_string(pack.path().join("crucible.toml")).expect("manifest"),
            "[repo]\nurl = \"x\"\n"
        );
        assert_eq!(
            std::fs::read_to_string(pack.path().join("gates").join("judge.py")).expect("nested"),
            "print(1)\n"
        );
        assert!(
            !pack.path().join(".git").exists(),
            ".git never rides the tarball"
        );
        assert!(!pack.path().join("STEER.md").exists(), "no steering rows");
    }

    #[cfg(feature = "autoresearch")]
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn missing_pack_is_none_and_or_empty_gives_a_bare_tree(pool: sqlx::PgPool) {
        assert!(
            materialize_pack(&pool, "owner/repo#404")
                .await
                .expect("materialize")
                .is_none()
        );
        let pack = materialize_pack_or_empty(&pool, "owner/repo#404")
            .await
            .expect("or_empty");
        assert!(pack.path().is_dir());
        assert_eq!(
            std::fs::read_dir(pack.path()).expect("read").count(),
            0,
            "empty tree"
        );
        assert!(
            read_pack_file(&pool, "owner/repo#404", "SCOPE.md")
                .await
                .expect("read")
                .is_none()
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn steering_rows_inject_onto_steer_md_in_order_and_deterministically(pool: sqlx::PgPool) {
        let tree = sample_tree();
        std::fs::write(tree.path().join("STEER.md"), "frozen guidance\n").unwrap();
        store_pack_tree(&pool, "owner/repo#7", tree.path())
            .await
            .expect("store");
        crate::runs::blob_store::append_steering(
            &pool,
            "owner_repo_7",
            "hoist the dup check",
            Some("a"),
        )
        .await
        .expect("append");
        crate::runs::blob_store::append_steering(&pool, "owner_repo_7", "pick fail-closed", None)
            .await
            .expect("append");

        let read_steer = |p: &MaterializedPack| {
            std::fs::read_to_string(p.path().join("STEER.md")).expect("STEER.md")
        };
        let first = materialize_pack(&pool, "owner/repo#7")
            .await
            .expect("materialize")
            .expect("stored");
        let text = read_steer(&first);
        assert!(text.starts_with("frozen guidance\n"), "{text}");
        assert_eq!(text.matches("by control -->").count(), 2);
        let dup = text.find("hoist the dup check").expect("first entry");
        let fail = text.find("pick fail-closed").expect("second entry");
        assert!(dup < fail, "seq order preserved");

        // Same tarball + rows → the same bytes, so build watch digests stay stable.
        let second = materialize_pack(&pool, "owner/repo#7")
            .await
            .expect("materialize")
            .expect("stored");
        assert_eq!(text, read_steer(&second));
    }

    #[cfg(feature = "autoresearch")]
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_traversal_tarball_is_refused_before_it_is_stored(pool: sqlx::PgPool) {
        use std::io::Write as _;
        // `tar::Builder::append_data` itself refuses `..` paths, so the hostile archive is
        // crafted at the raw-header level — what a malicious pod could emit.
        let path = "../escape.txt";
        let mut header = tar::Header::new_gnu();
        let name = &mut header.as_gnu_mut().expect("gnu header").name;
        name[..path.len()].copy_from_slice(path.as_bytes());
        header.set_size(4);
        header.set_mode(0o644);
        header.set_cksum();
        let mut builder = tar::Builder::new(Vec::new());
        builder.append(&header, "evil".as_bytes()).unwrap();
        let tar_bytes = builder.into_inner().unwrap();
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        let evil = enc.finish().unwrap();

        let err = store_pack_tarball(&pool, "owner/repo#7", &evil)
            .await
            .expect_err("traversal refused");
        assert!(format!("{err:#}").contains("escapes"), "{err:#}");
        assert!(
            crate::runs::blob_store::get_pack_tarball(&pool, "owner_repo_7")
                .await
                .expect("get")
                .is_none(),
            "nothing was stored"
        );
    }

    #[cfg(feature = "autoresearch")]
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn read_pack_file_scans_the_tarball_in_memory(pool: sqlx::PgPool) {
        let tree = sample_tree();
        store_pack_tree(&pool, "owner/repo#7", tree.path())
            .await
            .expect("store");
        assert_eq!(
            read_pack_file(&pool, "owner/repo#7", "SCOPE.md")
                .await
                .expect("read"),
            Some("identity: v1:beef\n".to_string())
        );
        assert_eq!(
            read_pack_file(&pool, "owner/repo#7", "gates/judge.py")
                .await
                .expect("read"),
            Some("print(1)\n".to_string())
        );
        assert!(
            read_pack_file(&pool, "owner/repo#7", "absent.md")
                .await
                .expect("read")
                .is_none()
        );
    }

    /// A tree within the budget fits; one whose delivered tarball is over it is refused with the
    /// size, and an empty tree (a draft not yet written) delivers nothing and fits.
    #[test]
    fn over_delivery_budget_names_an_oversize_tree() {
        use crate::testing::fixtures::{WORKFLOW_TOPIC, incompressible_text, write_playbook_pack};
        let dir = tempfile::tempdir().expect("tempdir");
        let pack = write_playbook_pack(dir.path(), WORKFLOW_TOPIC);
        assert_eq!(over_delivery_budget(&pack).expect("sized"), None);

        for seed in 1..=3 {
            std::fs::write(
                pack.join(format!("blob{seed}.txt")),
                incompressible_text(500 * 1024, seed),
            )
            .expect("blob");
        }
        let over = over_delivery_budget(&pack)
            .expect("sized")
            .expect("over the budget");
        assert!(over.contains("delivery budget"), "{over}");

        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).expect("mkdir");
        assert_eq!(over_delivery_budget(&empty).expect("sized"), None);
    }

    fn pack(files: &[(&str, &[u8])]) -> PackTree {
        PackTree::new(
            files
                .iter()
                .map(|(p, b)| (p.parse().expect("path"), b.to_vec()))
                .collect(),
        )
        .expect("tree")
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
                    .expect("append link"),
                None => builder
                    .append_data(&mut header, path, *bytes)
                    .expect("append"),
            }
        }
        builder.into_inner().expect("tar").finish().expect("gzip")
    }

    #[test]
    fn a_tree_round_trips_through_its_legacy_tarball() {
        let tree = pack(&[("crucible.toml", b"m"), ("tools/run.sh", b"#!/bin/sh\n")]);
        let ingested =
            tree_from_tar_gz(&encode_legacy_tar_gz(&tree).expect("encode")).expect("ingest");
        assert_eq!(ingested.tree.digest(), tree.digest());
        assert!(ingested.ignored.is_empty());
    }

    /// Two archives of the same files that differ in modes, timestamps, gzip level, and `./`
    /// prefixes name one pack.
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
        let (a, b) = (
            tree_from_tar_gz(&a).expect("a"),
            tree_from_tar_gz(&b).expect("b"),
        );
        assert_eq!(a.tree.digest(), b.tree.digest());
        assert_eq!(
            a.tree.digest(),
            pack(&[("crucible.toml", b"m"), ("tools/run.sh", b"x")]).digest()
        );
    }

    /// What the controller stored before tree storage, `tar_pack_tree` of a directory with a
    /// state dir, ingests to the same tree a fresh walk of the directory gives.
    #[test]
    fn a_legacy_stored_tarball_ingests_to_the_walked_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("tools")).expect("mkdir");
        std::fs::create_dir_all(root.join("state")).expect("mkdir");
        std::fs::write(root.join("crucible.toml"), "m").expect("write");
        std::fs::write(root.join("tools/run.sh"), "r").expect("write");
        std::fs::write(root.join("state/session.jsonl"), "s").expect("write");

        let ingested = tree_from_tar_gz(&tar_pack_tree(root).expect("legacy tar")).expect("ingest");

        assert_eq!(
            ingested.tree.digest(),
            walk_dir(root).expect("walk").tree.digest()
        );
        assert_eq!(ingested.ignored, vec!["state"]);
    }

    #[test]
    fn excluded_entries_are_dropped_and_reported_whatever_their_type() {
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
        let ingested = tree_from_tar_gz(&tgz).expect("ingest");
        assert_eq!(
            ingested.tree.digest(),
            pack(&[("crucible.toml", b"m")]).digest()
        );
        assert_eq!(ingested.ignored, vec!["state", "workspace", "x/.git"]);
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
            let err = tree_from_tar_gz(&tgz).expect_err(named).to_string();
            assert!(err.contains(named), "{named}: {err}");
        }
    }

    #[test]
    fn an_escaping_entry_is_refused() {
        // The tar crate refuses to write a `..` path, so set the name bytes by hand.
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(1);
        header.as_gnu_mut().expect("gnu").name[..7].copy_from_slice(b"../evil");
        header.set_cksum();
        builder.append(&header, &b"x"[..]).expect("append");
        let tgz = builder.into_inner().expect("tar").finish().expect("gzip");

        let err = tree_from_tar_gz(&tgz).expect_err("escapes").to_string();
        assert!(err.contains("escapes"), "{err}");
    }

    #[test]
    fn read_tree_reads_what_the_digest_covers() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("state")).expect("mkdir");
        std::fs::write(dir.path().join("crucible.toml"), "m").expect("write");
        std::fs::write(dir.path().join("state/s"), "s").expect("write");
        assert_eq!(
            read_tree(dir.path()).expect("read"),
            BTreeMap::from([("crucible.toml".to_string(), "m".to_string())])
        );

        std::os::unix::fs::symlink("crucible.toml", dir.path().join("link")).expect("symlink");
        assert!(matches!(
            read_tree(dir.path()),
            Err(ReadTreeError::NotAPack(TreeError::Symlink { .. }))
        ));
    }
}
