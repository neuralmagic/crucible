//! Pack storage over Postgres: the durable form of a frozen scope pack is the gzipped tarball in
//! `pack_tarballs` (keyed by the sanitized issue key), and human steering lives beside it as
//! `pack_steering` rows. Readers that need a working tree — the engine's PR push, `crucible
//! deploy render`, `[build]` planning — call [`materialize_pack`], which unpacks the tarball into
//! a scratch [`tempfile::TempDir`] and injects the steering rows onto `STEER.md`, in the exact
//! marker-wrapped shape `crucible/src/control.rs::append_steer` writes. Readers that need one
//! file (`SCOPE.md`) scan the tarball in memory via [`read_pack_file`].

#![allow(clippy::disallowed_macros)]

use anyhow::{Context, Result, bail};
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
    Io(#[from] anyhow::Error),
}

/// Read a pack tree back as a `{path: content}` map. A non-UTF-8 file is refused rather than
/// dropped: an editor that round-trips the whole map on every save would delete a file it
/// cannot show.
pub(crate) fn read_tree(root: &Path) -> Result<BTreeMap<String, String>, ReadTreeError> {
    let mut files = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?;
        for entry in entries {
            let entry = entry.context("reading a pack tree entry")?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let bytes = std::fs::read(&path).with_context(|| format!("reading {rel}"))?;
            let text = String::from_utf8(bytes).map_err(|_| ReadTreeError::NotText(rel.clone()))?;
            files.insert(rel, text);
        }
    }
    Ok(files)
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
}
