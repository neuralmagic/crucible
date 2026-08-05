//! Builds a [`RunIdentity`] from a loaded manifest: resolves the workspace HEAD sha, hashes the
//! frozen manifest text and the resolved injects, then hands the digest inputs to
//! `RunIdentity::new`.
//!
//! **Split from [`crate::loop_driver`]'s segment fingerprint.** The fingerprint tracks the
//! *regime* within a run (goal + objective + regime label; changes on a re-scope). `RunIdentity`
//! tracks the *world* across runs (source + manifest + frozen inputs + judge + deployment; changes on a
//! re-compose). Neither hashes the other's inputs, they share only the FNV-1a primitive
//! ([`fnv1a_hex`]).

use crate::manifest;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub use crucible_contract::identity::*;

/// The commit HEAD of a workspace repo, or `None` when it can't be read (no repo yet, detached
/// oddities), the caller falls back to an empty string rather than failing the run over an
/// identity nicety.
fn workspace_head_sha(workspace: &Path) -> Option<String> {
    let repo = git2::Repository::open(workspace).ok()?;
    let head = repo.head().ok()?.peel_to_commit().ok()?;
    Some(head.id().to_string())
}

/// Hash each inject's source content + destination path, in manifest declaration order.
fn hash_injects(injects: &[(PathBuf, PathBuf, bool)]) -> Result<String> {
    let mut parts: Vec<Vec<u8>> = Vec::new();
    for (src, dst, _frozen) in injects {
        let content = std::fs::read(src)
            .with_context(|| format!("reading inject src {} for identity digest", src.display()))?;
        parts.push(content);
        parts.push(dst.to_string_lossy().into_owned().into_bytes());
    }
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    Ok(fnv1a_hex(&refs))
}

/// Hash the `[agent].seed_diff` content, empty when the manifest declares none. A declared seed
/// that can't be read is an error, not an empty hash: an unseeded digest must never be recorded
/// for a run that meant to be seeded.
fn hash_seed(manifest_dir: &Path, seed_diff: Option<&str>) -> Result<String> {
    let Some(rel) = seed_diff else {
        return Ok(String::new());
    };
    let path = manifest_dir.join(rel);
    let content = std::fs::read(&path).with_context(|| {
        format!(
            "reading [agent].seed_diff {} for identity digest",
            path.display()
        )
    })?;
    Ok(fnv1a_hex(&[&content]))
}

/// Build the identity for a single-domain run.
pub fn for_manifest(
    manifest_path: &Path,
    manifest_dir: &Path,
    workspace: &Path,
    m: &manifest::Manifest,
) -> Result<RunIdentity> {
    let repo = m
        .repo
        .url
        .clone()
        .or_else(|| m.repo.path.clone())
        .unwrap_or_default();
    let components = vec![ComponentIdentity {
        name: String::new(),
        repo,
        base_sha: workspace_head_sha(workspace).unwrap_or_default(),
    }];
    let manifest_text = manifest::frozen_manifest_text(manifest_path, workspace)?;
    let injects = m.resolved_injects(manifest_dir, workspace);
    let inject_hash = hash_injects(&injects)?;
    let seed_hash = hash_seed(manifest_dir, m.agent.seed_diff.as_deref())?;
    Ok(RunIdentity::new(
        components,
        fnv1a_hex(&[manifest_text.as_bytes()]),
        inject_hash,
        seed_hash,
        m.judge.measure_cmd.clone(),
        m.judge.direction.clone(),
        RigIdentity::default(),
    ))
}

/// Build the identity for a composite run. `[workspace].inject` is unused at the composite level
/// (per-component setup owns its own), so `inject_hash` hashes an empty set.
pub fn for_composite(
    manifest_path: &Path,
    base_workspace: &Path,
    components: &[manifest::ResolvedComponent],
    m: &manifest::CompositeManifest,
) -> Result<RunIdentity> {
    let components = components
        .iter()
        .map(|c| ComponentIdentity {
            name: c.name.clone(),
            repo: c
                .manifest
                .repo
                .url
                .clone()
                .or_else(|| c.manifest.repo.path.clone())
                .unwrap_or_default(),
            base_sha: workspace_head_sha(&c.workspace).unwrap_or_default(),
        })
        .collect();
    let manifest_text = manifest::frozen_manifest_text(manifest_path, base_workspace)?;
    // `parent()` of a bare `crucible.toml` is `Some("")`; treat it as the current directory.
    let manifest_dir = manifest_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let seed_hash = hash_seed(manifest_dir, m.agent.seed_diff.as_deref())?;
    Ok(RunIdentity::new(
        components,
        fnv1a_hex(&[manifest_text.as_bytes()]),
        hash_injects(&[])?,
        seed_hash,
        m.judge.measure_cmd.clone(),
        m.judge.direction.clone(),
        RigIdentity::default(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "crucible-identity-test-{}-{n}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("mkdir tempdir");
        dir
    }

    fn init_repo(dir: &Path) -> String {
        let repo = git2::Repository::init(dir).expect("git init");
        fs::write(dir.join("f.txt"), "hello").expect("write");
        let mut index = repo.index().expect("index");
        index.add_path(Path::new("f.txt")).expect("add");
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let sig = git2::Signature::now("test", "test@example.com").expect("sig");
        let oid = repo
            .commit(Some("HEAD"), &sig, &sig, "root", &tree, &[])
            .expect("commit");
        oid.to_string()
    }

    fn manifest_with(dir: &Path, extra: &str) -> (PathBuf, manifest::Manifest) {
        let manifest_path = dir.join("crucible.toml");
        let text = format!(
            r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "m"
            direction = "higher"
            [agent]
            backend = "command"
            agent_cmd = "a"
            goal = "g"
            {extra}
            "#
        );
        fs::write(&manifest_path, &text).expect("write manifest");
        let m: manifest::Manifest = toml::from_str(&text).expect("parse manifest");
        (manifest_path, m)
    }

    #[test]
    fn digest_is_deterministic_and_input_sensitive() {
        let dir = tempdir();
        let sha = init_repo(&dir);
        let (manifest_path, m) = manifest_with(&dir, "");

        let a = for_manifest(&manifest_path, &dir, &dir, &m).expect("identity a");
        let b = for_manifest(&manifest_path, &dir, &dir, &m).expect("identity b");
        assert_eq!(a.digest, b.digest, "same inputs must yield the same digest");
        assert_eq!(a.components[0].base_sha, sha);
        assert!(a.digest.starts_with("v2:"));

        // Change one input (measure_cmd), the digest must move.
        let (manifest_path2, m2) = manifest_with(&dir, "");
        let mut m2 = m2;
        m2.judge.measure_cmd = "different".to_string();
        let c = for_manifest(&manifest_path2, &dir, &dir, &m2).expect("identity c");
        assert_ne!(
            a.digest, c.digest,
            "a changed measure_cmd must change the digest"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base_sha_change_moves_the_digest() {
        let dir = tempdir();
        init_repo(&dir);
        let (manifest_path, m) = manifest_with(&dir, "");
        let a = for_manifest(&manifest_path, &dir, &dir, &m).expect("identity a");

        // Advance the workspace to a new commit.
        let repo = git2::Repository::open(&dir).expect("open");
        fs::write(dir.join("f.txt"), "changed").expect("write");
        let mut index = repo.index().expect("index");
        index.add_path(Path::new("f.txt")).expect("add");
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let sig = git2::Signature::now("test", "test@example.com").expect("sig");
        let parent = repo.head().expect("head").peel_to_commit().expect("commit");
        repo.commit(Some("HEAD"), &sig, &sig, "edit", &tree, &[&parent])
            .expect("commit");

        let b = for_manifest(&manifest_path, &dir, &dir, &m).expect("identity b");
        assert_ne!(
            a.digest, b.digest,
            "a moved base SHA must change the digest"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inject_content_change_moves_the_digest() {
        let dir = tempdir();
        init_repo(&dir);
        let judge_src = dir.join("judge.txt");
        fs::write(&judge_src, "v1").expect("write judge src");
        let extra = r#"
            [[workspace.inject]]
            src = "judge.txt"
            dst = "judge.txt"
        "#;
        let (manifest_path, m) = manifest_with(&dir, extra);
        let a = for_manifest(&manifest_path, &dir, &dir, &m).expect("identity a");

        fs::write(&judge_src, "v2").expect("edit judge src");
        let b = for_manifest(&manifest_path, &dir, &dir, &m).expect("identity b");
        assert_ne!(
            a.digest, b.digest,
            "a changed inject source must change the digest"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_diff_moves_the_digest() {
        let dir = tempdir();
        init_repo(&dir);
        let (manifest_path, m) = manifest_with(&dir, "");
        let unseeded = for_manifest(&manifest_path, &dir, &dir, &m).expect("unseeded identity");
        assert!(unseeded.seed_hash.is_empty());

        let seed_src = dir.join("seed.diff");
        fs::write(&seed_src, "--- a\n+++ b\n").expect("write seed");
        let (manifest_path, m) = manifest_with(&dir, r#"seed_diff = "seed.diff""#);
        let seeded = for_manifest(&manifest_path, &dir, &dir, &m).expect("seeded identity");
        assert!(!seeded.seed_hash.is_empty());
        assert_ne!(
            unseeded.digest, seeded.digest,
            "declaring a seed must change the digest"
        );

        fs::write(&seed_src, "--- a\n+++ c\n").expect("edit seed");
        let reseeded = for_manifest(&manifest_path, &dir, &dir, &m).expect("reseeded identity");
        assert_ne!(
            seeded.digest, reseeded.digest,
            "changed seed content must change the digest"
        );

        // A declared seed that can't be read is an error, never an unseeded digest.
        fs::remove_file(&seed_src).expect("remove seed");
        assert!(for_manifest(&manifest_path, &dir, &dir, &m).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn composite_component_ordering_is_stable() {
        let base = tempdir();
        let a_dir = base.join("a");
        let b_dir = base.join("b");
        fs::create_dir_all(&a_dir).expect("mkdir a");
        fs::create_dir_all(&b_dir).expect("mkdir b");
        let sha_a = init_repo(&a_dir);
        let sha_b = init_repo(&b_dir);

        let comp_a = manifest::Manifest::load(&{
            let p = a_dir.join("crucible.toml");
            fs::write(
                &p,
                r#"[repo]
                path = "."
                [judge]
                measure_cmd = "m"
                direction = "higher"
                [agent]
                backend = "command"
                agent_cmd = "a"
                goal = "g"
                "#,
            )
            .expect("write");
            p
        })
        .expect("load a manifest");
        let comp_b = manifest::Manifest::load(&{
            let p = b_dir.join("crucible.toml");
            fs::write(
                &p,
                r#"[repo]
                path = "."
                [judge]
                measure_cmd = "m"
                direction = "higher"
                [agent]
                backend = "command"
                agent_cmd = "a"
                goal = "g"
                "#,
            )
            .expect("write");
            p
        })
        .expect("load b manifest");

        let resolved = vec![
            manifest::ResolvedComponent {
                name: "a".to_string(),
                domain_dir: a_dir.clone(),
                manifest: comp_a,
                workspace: a_dir.clone(),
            },
            manifest::ResolvedComponent {
                name: "b".to_string(),
                domain_dir: b_dir.clone(),
                manifest: comp_b,
                workspace: b_dir.clone(),
            },
        ];

        let composite_path = base.join("crucible.toml");
        let composite_text = r#"
            [composite]
            name = "test"
            [[component]]
            domain = "a"
            [[component]]
            domain = "b"
            [agent]
            backend = "command"
            agent_cmd = "a"
            goal = "g"
            [judge]
            measure_cmd = "m"
            direction = "higher"
        "#;
        fs::write(&composite_path, composite_text).expect("write composite manifest");
        let cm: manifest::CompositeManifest =
            toml::from_str(composite_text).expect("parse composite manifest");

        let first = for_composite(&composite_path, &base, &resolved, &cm).expect("identity");
        let second = for_composite(&composite_path, &base, &resolved, &cm).expect("identity");
        assert_eq!(
            first.digest, second.digest,
            "rebuilding the same vector is stable"
        );
        assert_eq!(first.components.len(), 2);
        assert_eq!(first.components[0].base_sha, sha_a);
        assert_eq!(first.components[1].base_sha, sha_b);

        let _ = std::fs::remove_dir_all(&base);
    }
}
