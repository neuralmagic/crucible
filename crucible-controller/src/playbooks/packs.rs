//! Pack storage over Postgres: a frozen scope pack is the stored tree its `pack_tarballs` row
//! (keyed by the sanitized issue key) names, beside the gzipped tarball older controllers read,
//! and human steering lives beside it as `pack_steering` rows. Readers that need a working tree
//! (the engine's PR push, `crucible deploy render`, `[build]` planning) call [`materialize_pack`],
//! which writes exactly the stored tree into a scratch [`tempfile::TempDir`]. A run receives the
//! steering rows beside that tree as [`steer_md`], in the exact marker-wrapped shape
//! `crucible/src/control.rs::append_steer` writes. Readers that need one file (`SCOPE.md`) read
//! it alone via [`read_pack_file`].

#![allow(clippy::disallowed_macros)]

use crate::runs::blob_store::StoredPack;
use anyhow::{Context, Result};
use crucible_contract::pack_tree::{
    OverBudget, PackTree, ReadPack, TreeDigest, TreeError, check_delivery_budget, walk_dir,
};
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A pack tree written to scratch. The tempdir guard lives inside, so the tree exists exactly as
/// long as this value — keep it alive for the duration of any subprocess reading [`Self::path`].
pub struct MaterializedPack {
    _guard: tempfile::TempDir,
    root: PathBuf,
    digest: TreeDigest,
}

impl MaterializedPack {
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// The digest of the tree written at [`Self::path`].
    pub fn digest(&self) -> &TreeDigest {
        &self.digest
    }
}

/// Why a pack tree is refused before it is stored.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PackRefusal {
    #[error(transparent)]
    NotAPack(#[from] TreeError),
    #[error(transparent)]
    OverBudget(#[from] OverBudget),
    #[error("encoding the pack tarball: {0}")]
    Encode(#[from] std::io::Error),
}

/// A pack ready to store: its tree, the tarball a run receives, and the excluded paths the read
/// skipped.
#[derive(Debug, Clone)]
pub(crate) struct Deliverable {
    pub tree: PackTree,
    pub tarball: Vec<u8>,
    pub ignored: Vec<String>,
}

impl Deliverable {
    /// Encode `read`, refused when its tarball is over the delivery budget
    /// (RFC-0002:C-PACK-BASE).
    pub(crate) fn new(read: ReadPack) -> Result<Self, PackRefusal> {
        let tarball = read.tree.tarball()?;
        check_delivery_budget(&tarball)?;
        Ok(Self {
            tree: read.tree,
            tarball,
            ignored: read.ignored,
        })
    }
}

/// The pack at `root`, refused when it is not a pack or is over the delivery budget.
pub(crate) fn deliverable(root: &Path) -> Result<Deliverable, PackRefusal> {
    Deliverable::new(walk_dir(root)?)
}

/// Gzip-tar a pack working tree the way the controller stored packs before tree storage.
#[cfg(test)]
pub(crate) fn tar_pack_tree(tree: &Path) -> Result<Vec<u8>> {
    let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);
    append_dir(&mut builder, tree, Path::new(""))?;
    let enc = builder.into_inner().context("finishing the pack tar")?;
    enc.finish().context("gzipping the pack tar")
}

#[cfg(test)]
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

/// Store `pack` as `key`'s durable pack.
pub(crate) async fn store_pack(pool: &PgPool, key: &str, pack: &Deliverable) -> Result<StoredPack> {
    let mut conn = pool.acquire().await.context("pack store connection")?;
    crate::runs::blob_store::put_pack(
        &mut conn,
        &crate::model::sanitize_key(key),
        &pack.tree,
        &pack.tarball,
    )
    .await
}

/// Store a pod-delivered pack tarball as `key`'s durable pack. The tarball is read as a pack
/// first, so a hostile, non-pack, or over-budget blob is refused before anything is stored.
#[cfg(feature = "autoresearch")]
pub(crate) async fn store_pack_tarball(
    pool: &PgPool,
    key: &str,
    tar_gz: &[u8],
) -> Result<StoredPack> {
    let tar_gz = tar_gz.to_vec();
    let pack = tokio::task::spawn_blocking(move || {
        Deliverable::new(crucible_contract::pack_tree::read_tar_gz(&tar_gz)?)
    })
    .await
    .context("joining the pack read worker")?
    .context("reading the pack tarball")?;
    store_pack(pool, key, &pack).await
}

/// Store the pack in a local directory as `key`'s durable pack, refused when it is not a pack or
/// is over the delivery budget.
pub async fn store_pack_tree(pool: &PgPool, key: &str, dir: &Path) -> Result<StoredPack> {
    let root = dir.to_path_buf();
    let pack = tokio::task::spawn_blocking(move || deliverable(&root))
        .await
        .context("joining the pack read worker")?
        .with_context(|| format!("reading the pack at {}", dir.display()))?;
    store_pack(pool, key, &pack).await
}

/// Write `tree` into `dir` (created if missing), every file with mode 0755 as the pod's pack mount
/// gives it.
pub(crate) fn write_tree(tree: &PackTree, dir: &Path) -> Result<()> {
    write_files(tree.files(), dir)
}

/// Write `tree` into `dir`, refused unless the directory then reads back as exactly `tree`
/// (RFC-0002:C-PACK-BASE).
pub(crate) fn write_checked_tree(tree: &PackTree, dir: &Path) -> Result<()> {
    write_tree(tree, dir)?;
    let written = walk_dir(dir)
        .with_context(|| format!("reading back the pack at {}", dir.display()))?
        .tree
        .digest();
    let expected = tree.digest();
    if written != expected {
        anyhow::bail!(
            "the pack written at {} reads back as tree {written}, not the stored tree {expected}",
            dir.display()
        );
    }
    Ok(())
}

/// Write `files` under `dir` (created if missing), each with mode 0755.
pub(crate) fn write_files(
    files: &BTreeMap<crucible_contract::pack_tree::PackFilePath, Vec<u8>>,
    dir: &Path,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    for (path, bytes) in files {
        let dest = dir.join(path.as_str());
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&dest, bytes).with_context(|| format!("writing {path}"))?;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("setting the mode of {path}"))?;
    }
    Ok(())
}

/// Write `tree` into a scratch directory. The tempdir guard rides the returned value.
pub(crate) fn materialize_tree(tree: &PackTree) -> Result<MaterializedPack> {
    let guard = tempfile::tempdir().context("pack materialization scratch dir")?;
    let root = guard.path().join("pack");
    write_tree(tree, &root)?;
    Ok(MaterializedPack {
        _guard: guard,
        root,
        digest: tree.digest(),
    })
}

/// Write exactly `key`'s stored pack into a scratch tree. `None` when no pack was ever stored.
pub(crate) async fn materialize_pack(pool: &PgPool, key: &str) -> Result<Option<MaterializedPack>> {
    let slug = crate::model::sanitize_key(key);
    let Some(tree) = crate::runs::blob_store::get_pack(pool, &slug).await? else {
        return Ok(None);
    };
    tokio::task::spawn_blocking(move || materialize_tree(&tree))
        .await
        .context("joining the pack materialization worker")?
        .with_context(|| format!("writing the stored pack for {key}"))
        .map(Some)
}

/// The `STEER.md` a run of `key` receives beside the pack written at `pack_dir`: the pack's own
/// `STEER.md` with `key`'s steering rows appended, ordered by seq and each stamped with the row's
/// recorded time, so the same pack and rows always give the same bytes. `None` when `key` has no
/// steering rows.
pub(crate) async fn steer_md(pool: &PgPool, key: &str, pack_dir: &Path) -> Result<Option<Vec<u8>>> {
    let slug = crate::model::sanitize_key(key);
    let steering = crate::runs::blob_store::list_steering(pool, &slug).await?;
    if steering.is_empty() {
        return Ok(None);
    }
    let mut out = match tokio::fs::read(pack_dir.join(STEER_MD)).await {
        Ok(frozen) => frozen,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).context("reading the pack's STEER.md"),
    };
    for s in &steering {
        let ts: jiff::Timestamp = s
            .created_at
            .parse()
            .with_context(|| format!("parsing steering timestamp {:?}", s.created_at))?;
        out.extend_from_slice(
            format!(
                "<!-- steer @{} by control -->\n{}\n",
                ts.as_second(),
                s.body_md.trim()
            )
            .as_bytes(),
        );
    }
    Ok(Some(out))
}

/// The pack-relative path a run reads steering from.
pub(crate) const STEER_MD: &str = "STEER.md";

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
    materialize_tree(&PackTree::default())
}

/// Read one file (by pack-relative path) of `key`'s stored pack. `None` when no pack is stored or
/// the file isn't in it.
#[cfg(feature = "autoresearch")]
pub(crate) async fn read_pack_file(pool: &PgPool, key: &str, name: &str) -> Result<Option<String>> {
    use crate::playbooks::pack_trees::PackRef;
    let slug = crate::model::sanitize_key(key);
    let row = sqlx::query(
        "SELECT tree_digest, CASE WHEN tree_digest IS NULL THEN tar_gz END AS tar_gz
         FROM pack_tarballs WHERE issue_slug = $1",
    )
    .bind(&slug)
    .fetch_optional(pool)
    .await
    .context("reading a stored pack")?;
    let bytes = match row.as_ref().map(PackRef::from_row).transpose()?.flatten() {
        None => return Ok(None),
        Some(PackRef::Tree(digest)) => {
            crate::playbooks::pack_trees::read_file(pool, &digest, name).await?
        }
        Some(legacy) => {
            let tree = crate::playbooks::pack_trees::load(pool, legacy).await?;
            name.parse::<crucible_contract::pack_tree::PackFilePath>()
                .ok()
                .and_then(|path| tree.into_files().remove(&path))
        }
    };
    bytes
        .map(String::from_utf8)
        .transpose()
        .with_context(|| format!("{name} in the stored pack for {key} is not text"))
}

/// Why a pack tree could not be read back as text files.
#[derive(Debug, thiserror::Error)]
pub enum ReadTreeError {
    #[error("{0} is not text")]
    NotText(String),
    #[error(transparent)]
    NotAPack(#[from] TreeError),
}

/// `tree` as a `{path: content}` map. A non-UTF-8 file is refused rather than dropped: an editor
/// that round-trips the whole map on every save would delete a file it cannot show.
pub(crate) fn text_files(tree: PackTree) -> Result<BTreeMap<String, String>, ReadTreeError> {
    tree.into_files()
        .into_iter()
        .map(|(path, bytes)| {
            let path = String::from(path);
            String::from_utf8(bytes)
                .map(|text| (path.clone(), text))
                .map_err(|_| ReadTreeError::NotText(path))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::playbooks::packs::*;

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

    /// A scope pack is stored as its tree beside that tree's own tarball, whether it arrives as a
    /// directory or as a pod-delivered tarball in any encoding.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_stored_scope_pack_records_its_tree(pool: sqlx::PgPool) {
        let dir = sample_tree();
        let tree = walk_dir(dir.path()).expect("walk").tree;
        let tarball = tree.tarball().expect("tarball");
        let stored_row = || async {
            sqlx::query_as::<_, (Option<String>, Vec<u8>, String)>(
                "SELECT tree_digest, tar_gz, digest FROM pack_tarballs WHERE issue_slug = $1",
            )
            .bind("owner_repo_7")
            .fetch_one(&pool)
            .await
            .expect("pack row")
        };
        let expected = (
            Some(tree.digest().to_string()),
            tarball.clone(),
            crucible_contract::content_digest(&tarball),
        );

        let stored = store_pack_tree(&pool, "owner/repo#7", dir.path())
            .await
            .expect("store");
        assert_eq!(stored.tree, tree.digest());
        assert_eq!(stored_row().await, expected);

        #[cfg(feature = "autoresearch")]
        {
            sqlx::query("DELETE FROM pack_tarballs")
                .execute(&pool)
                .await
                .expect("clear");
            let legacy = tar_pack_tree(dir.path()).expect("legacy tar");
            assert_ne!(legacy, tarball, "a different encoding of the same tree");
            store_pack_tarball(&pool, "owner/repo#7", &legacy)
                .await
                .expect("store");
            assert_eq!(stored_row().await, expected);
        }
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

    /// Steering rows never enter the materialized tree; they are appended onto the pack's own
    /// `STEER.md` in the copy a run receives beside it, in seq order and the same bytes each time.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn steering_rows_append_onto_steer_md_beside_the_tree(pool: sqlx::PgPool) {
        let tree = sample_tree();
        std::fs::write(tree.path().join("STEER.md"), "frozen guidance\n").unwrap();
        store_pack_tree(&pool, "owner/repo#7", tree.path())
            .await
            .expect("store");
        let stored = walk_dir(tree.path()).expect("walk").tree;
        let pack = materialize_pack(&pool, "owner/repo#7")
            .await
            .expect("materialize")
            .expect("stored");
        assert_eq!(
            steer_md(&pool, "owner/repo#7", pack.path())
                .await
                .expect("steer"),
            None,
            "no rows, nothing beside the tree"
        );
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

        let pack = materialize_pack(&pool, "owner/repo#7")
            .await
            .expect("materialize")
            .expect("stored");
        assert_eq!(walk_dir(pack.path()).expect("walk").tree, stored);
        assert_eq!(pack.digest(), &stored.digest());
        let first = steer_md(&pool, "owner/repo#7", pack.path())
            .await
            .expect("steer")
            .expect("rows");
        let text = String::from_utf8(first.clone()).expect("text");
        assert!(text.starts_with("frozen guidance\n"), "{text}");
        assert_eq!(text.matches("by control -->").count(), 2);
        let dup = text.find("hoist the dup check").expect("first entry");
        let fail = text.find("pick fail-closed").expect("second entry");
        assert!(dup < fail, "seq order preserved");

        let bare = materialize_tree(&PackTree::default()).expect("empty");
        let second = steer_md(&pool, "owner/repo#7", bare.path())
            .await
            .expect("steer")
            .expect("rows");
        assert_eq!(
            [b"frozen guidance\n".as_slice(), &second].concat(),
            first,
            "a pack without STEER.md receives the rows alone"
        );
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
            crate::runs::blob_store::get_pack(&pool, "owner_repo_7")
                .await
                .expect("get")
                .is_none(),
            "nothing was stored"
        );
    }

    /// Bytes gzip cannot shrink, so a tree of them delivers about its own size.
    #[cfg(feature = "autoresearch")]
    fn incompressible(len: usize) -> Vec<u8> {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[cfg(feature = "autoresearch")]
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_oversize_pack_is_refused_before_it_is_stored(pool: sqlx::PgPool) {
        let budget = crucible_contract::pack_tree::DELIVERY_BUDGET_BYTES;
        let over_budget = PackTree::from_pairs(&[("blob", &incompressible(budget + 4096))])
            .expect("tree")
            .tarball()
            .expect("tarball");
        let expansion = crucible_contract::pack_tree::MAX_EXPANDED_BYTES as usize;
        let bomb = PackTree::from_pairs(&[("zeros", &vec![0u8; expansion + 1])])
            .expect("tree")
            .tarball()
            .expect("tarball");
        assert!(bomb.len() < budget, "{} gzipped bytes", bomb.len());
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("blob"), incompressible(budget + 4096)).expect("write");

        let refusals = [
            store_pack_tarball(&pool, "owner/repo#7", &over_budget)
                .await
                .expect_err("over budget"),
            store_pack_tarball(&pool, "owner/repo#7", &bomb)
                .await
                .expect_err("expands too far"),
            store_pack_tree(&pool, "owner/repo#7", dir.path())
                .await
                .expect_err("over budget on disk"),
        ];

        for (err, says) in
            refusals
                .iter()
                .zip(["delivery budget", "expands past", "delivery budget"])
        {
            assert!(format!("{err:#}").contains(says), "{err:#}");
        }
        let stored: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM pack_tarballs) + (SELECT count(*) FROM pack_trees)",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(stored, 0, "nothing was stored");
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

    /// A pack within the budget yields the tarball a run receives; one over it is refused with
    /// its size, and one holding a symlink is refused as not a pack.
    #[test]
    fn deliverable_refuses_an_oversize_or_invalid_pack() {
        use crate::testing::fixtures::{
            WORKFLOW_TOPIC, write_over_budget_blobs, write_playbook_pack,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let pack = write_playbook_pack(dir.path(), WORKFLOW_TOPIC);
        let delivered = deliverable(&pack).expect("within budget");
        let walked = walk_dir(&pack).expect("walk").tree;
        assert_eq!(delivered.tarball, walked.tarball().expect("tarball"));
        assert_eq!(delivered.tree, walked);

        std::os::unix::fs::symlink("crucible.toml", pack.join("link")).expect("symlink");
        assert!(matches!(
            deliverable(&pack),
            Err(PackRefusal::NotAPack(TreeError::Symlink { .. }))
        ));
        std::fs::remove_file(pack.join("link")).expect("rm");

        write_over_budget_blobs(&pack);
        match deliverable(&pack) {
            Err(PackRefusal::OverBudget(over)) => {
                assert!(over.to_string().contains("delivery budget"), "{over}")
            }
            other => panic!("expected an over-budget refusal, got {other:?}"),
        }
    }

    /// What the controller stored before tree storage, `tar_pack_tree` of a directory with a
    /// state dir, reads back as the tree a fresh walk of the directory gives.
    #[test]
    fn a_legacy_stored_tarball_reads_as_the_walked_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("tools")).expect("mkdir");
        std::fs::create_dir_all(root.join("state")).expect("mkdir");
        std::fs::write(root.join("crucible.toml"), "m").expect("write");
        std::fs::write(root.join("tools/run.sh"), "r").expect("write");
        std::fs::write(root.join("state/session.jsonl"), "s").expect("write");

        let read =
            crucible_contract::pack_tree::read_tar_gz(&tar_pack_tree(root).expect("legacy tar"))
                .expect("read");

        assert_eq!(read.tree, walk_dir(root).expect("walk").tree);
        assert_eq!(read.ignored, vec!["state"]);
    }

    #[test]
    fn text_files_refuses_a_file_that_is_not_text() {
        let text = PackTree::from_pairs(&[("crucible.toml", b"m")]).expect("tree");
        assert_eq!(
            text_files(text).expect("text"),
            BTreeMap::from([("crucible.toml".to_string(), "m".to_string())])
        );
        let binary = PackTree::from_pairs(&[("blob", &[0xff, 0xfe])]).expect("tree");
        assert!(matches!(
            text_files(binary),
            Err(ReadTreeError::NotText(path)) if path == "blob"
        ));
    }

    /// A written tree holds exactly the tree's files, each mode 0755 as the pod's pack mount
    /// gives it.
    #[test]
    fn a_written_tree_walks_back_to_itself_with_every_file_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tree = PackTree::from_pairs(&[("role.sh", b"#!/bin/sh\n"), ("inbox/a.md", b"a")])
            .expect("tree");
        let pack = materialize_tree(&tree).expect("materialize");
        assert_eq!(walk_dir(pack.path()).expect("walk").tree, tree);
        for f in ["role.sh", "inbox/a.md"] {
            let mode = std::fs::metadata(pack.path().join(f))
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755, "{f}");
        }
    }
}
