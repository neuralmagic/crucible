//! A run's captured files: the declared task outputs a finished run exposes, and the content of
//! each ([`RFC-0001:C-RUN-OUTPUTS`]).
//!
//! Two backings, one shape. A pod run's files reach the controller as the `run-files` evidence
//! the pod uploads — one gzipped tar of the loop's `state/files`, adopted onto the run at completion and
//! read back out of [`crate::runs::blob_store`]. A local run's files never left this machine, so they are
//! walked off the scratch disk instead. Both answer with the same [`RunFile`] listing and the same
//! keys, so the API and the CLI never branch on dispatch.
//!
//! The tree is `state/files/<task>/<declared path>`, and the engine's `capture_declared` publishes
//! a task's directory by rename only once every file that task declared is present. A file the task
//! did not produce therefore has no entry at all, which is what the clause requires: absent, not
//! listed-and-empty.

use anyhow::{Context, Result};
use serde::Serialize;
use std::io::Read as _;
use std::path::Path;

/// One file a run's task captured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct RunFile {
    /// The declaring task's name, fan-out brackets stripped.
    pub task: String,
    /// The fan-out instance key as the run named it, for a file captured by an instance of a mapped
    /// task; null for a plain task, whose captures are the task's own.
    pub instance: Option<String>,
    /// The path the task declared, relative to its capture directory.
    pub path: String,
    pub size_bytes: u64,
    /// The opaque key that fetches this file's content; stable for the life of the run.
    pub key: String,
}

/// Split a capture directory name into the task and, for a fan-out instance, its raw key. The
/// engine names an instance `task[key]` and escapes the key where it forms the directory, so the
/// directory decodes back to the name the session log carries.
fn split_instance(dir: &str) -> (String, Option<String>) {
    let name = crucible::plan::ir::TaskName::from_dir_name(dir);
    match name.instance_of() {
        Some((task, key)) => (task.to_string(), Some(key.to_string())),
        None => (dir.to_string(), None),
    }
}

/// Whether a tar entry path is a plain relative `<task>/<declared>` with no escape. The tar is
/// written by the loop, but it arrives over the network, so a `..` or absolute component is
/// dropped rather than trusted.
fn safe_key(key: &str) -> bool {
    let components: Vec<&str> = key.split('/').collect();
    components.len() >= 2
        && components
            .iter()
            .all(|c| !c.is_empty() && *c != "." && *c != ".." && !c.contains('\\'))
}

/// Split a key into its capture directory and the declared path under it. The separator is the
/// first `/` OUTSIDE any bracket: a fan-out instance is keyed by the item it maps over, and that
/// item is routinely a path, so `check[docs/SUMMARY.md]/BROKEN.json` has a `/` inside the
/// instance's own name that is not a directory boundary.
fn split_dir(key: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    for (i, c) in key.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            '/' if depth == 0 => return Some((&key[..i], &key[i + 1..])),
            _ => {}
        }
    }
    None
}

/// Whether `key` is a well-formed `<task>/<declared>` file key, the shape a listing hands out
/// and a fetch accepts.
pub(crate) fn is_key(key: &str) -> bool {
    entry(key, 0).is_some()
}

/// Turn an entry path into a [`RunFile`], or `None` when the path is not a safe
/// `<task>/<declared>`.
fn entry(key: &str, size_bytes: u64) -> Option<RunFile> {
    if !safe_key(key) {
        return None;
    }
    let (dir, rest) = split_dir(key)?;
    if dir.is_empty() || rest.is_empty() {
        return None;
    }
    let (task, instance) = split_instance(dir);
    Some(RunFile {
        task,
        instance,
        path: rest.to_string(),
        size_bytes,
        key: key.to_string(),
    })
}

/// The files in a stored bundle, ordered by key so a listing is stable across calls.
pub fn list_bundle(tar_gz: &[u8]) -> Result<Vec<RunFile>> {
    let mut archive = tar::Archive::new(flate2::read::MultiGzDecoder::new(tar_gz));
    let mut files = Vec::new();
    for e in archive.entries().context("reading the run-files tar")? {
        let e = e.context("reading a run-files tar entry")?;
        if !e.header().entry_type().is_file() {
            continue;
        }
        let path = e.path().context("decoding a run-files tar entry path")?;
        let Some(key) = path.to_str() else {
            continue;
        };
        if let Some(file) = entry(key, e.header().size().unwrap_or(0)) {
            files.push(file);
        }
    }
    files.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(files)
}

/// One file's bytes out of a stored bundle, or `None` when the bundle holds no such key.
pub fn read_bundle(tar_gz: &[u8], key: &str) -> Result<Option<Vec<u8>>> {
    if !safe_key(key) {
        return Ok(None);
    }
    let mut archive = tar::Archive::new(flate2::read::MultiGzDecoder::new(tar_gz));
    for e in archive.entries().context("reading the run-files tar")? {
        let mut e = e.context("reading a run-files tar entry")?;
        if !e.header().entry_type().is_file() {
            continue;
        }
        let matches = e
            .path()
            .context("decoding a run-files tar entry path")?
            .to_str()
            == Some(key);
        if matches {
            let mut buf = Vec::new();
            e.read_to_end(&mut buf)
                .with_context(|| format!("reading {key} out of the run-files tar"))?;
            return Ok(Some(buf));
        }
    }
    Ok(None)
}

/// Every file one capture directory holds, each with its bytes when it is no bigger than
/// `inline_cap`: out of the stored bundle when the run has one, off the scratch disk otherwise.
/// `dir` is the directory as the engine named it, so a fan-out instance's key arrives escaped.
pub async fn dir_files(
    bundle: Option<&[u8]>,
    scratch_root: &Path,
    run_id: &str,
    dir: &str,
    inline_cap: u64,
) -> Result<Vec<(RunFile, Option<Vec<u8>>)>> {
    let Some(tar_gz) = bundle else {
        let mut files = Vec::new();
        for file in list_local(scratch_root, run_id).await {
            if split_dir(&file.key).map(|(d, _)| d) != Some(dir) {
                continue;
            }
            let body = if file.size_bytes <= inline_cap {
                read_local(scratch_root, run_id, &file.key).await
            } else {
                None
            };
            files.push((file, body));
        }
        return Ok(files);
    };
    let mut archive = tar::Archive::new(flate2::read::MultiGzDecoder::new(tar_gz));
    let mut files = Vec::new();
    for e in archive.entries().context("reading the run-files tar")? {
        let mut e = e.context("reading a run-files tar entry")?;
        if !e.header().entry_type().is_file() {
            continue;
        }
        let path = e.path().context("decoding a run-files tar entry path")?;
        let Some(key) = path.to_str().map(str::to_owned) else {
            continue;
        };
        if split_dir(&key).map(|(d, _)| d) != Some(dir) {
            continue;
        }
        let Some(file) = entry(&key, e.header().size().unwrap_or(0)) else {
            continue;
        };
        let body = if file.size_bytes <= inline_cap {
            let mut buf = Vec::new();
            e.read_to_end(&mut buf)
                .with_context(|| format!("reading {key} out of the run-files tar"))?;
            Some(buf)
        } else {
            None
        };
        files.push((file, body));
    }
    files.sort_by(|a, b| a.0.key.cmp(&b.0.key));
    Ok(files)
}

/// How many captured files one task's evidence lists.
const MAX_FILES: usize = 64;

/// One file a task captured into the run's state directory.
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedFile {
    /// The path the task declared, relative to its capture directory.
    pub name: String,
    pub size_bytes: u64,
    /// The file's text, for UTF-8 content under [`crate::runs::task_evidence::MAX_INLINE_BYTES`]; `None` for anything bigger
    /// or not decodable.
    pub content: Option<String>,
}

/// Every file one task captured, path-sorted, each with its size and — for decodable text under
/// the cap — its content. `bundle` is the run's stored `run-files` tar; without one, a local run's
/// own directory is read. A task that captured nothing and a traversal attempt both read the same:
/// no files.
pub async fn captured_files(
    bundle: Option<&[u8]>,
    scratch_root: &Path,
    run_id: &str,
    task: &str,
) -> anyhow::Result<Vec<CapturedFile>> {
    let Some(dir) = crate::runs::task_evidence::task_segment(task)
        .filter(|_| crate::runs::task_evidence::safe_segment(run_id))
    else {
        return Ok(Vec::new());
    };
    let mut files: Vec<CapturedFile> = dir_files(
        bundle,
        scratch_root,
        run_id,
        &dir,
        crate::runs::task_evidence::MAX_INLINE_BYTES,
    )
    .await?
    .into_iter()
    .map(|(file, body)| CapturedFile {
        name: file.path,
        size_bytes: file.size_bytes,
        content: body.and_then(|bytes| String::from_utf8(bytes).ok()),
    })
    .collect();
    files.truncate(MAX_FILES);
    Ok(files)
}

/// The files a local run captured, walked off the scratch disk. Empty for a run that captured
/// nothing, and for a pod run, whose files only ever existed on the pod.
pub async fn list_local(scratch_root: &Path, run_id: &str) -> Vec<RunFile> {
    if !crate::runs::task_evidence::safe_segment(run_id) {
        return Vec::new();
    }
    let root = crate::runs::task_evidence::files_root(scratch_root, run_id);
    let mut files = Vec::new();
    let mut stack = vec![(root.clone(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(e)) = entries.next_entry().await {
            let name = e.file_name().to_string_lossy().into_owned();
            let key = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            // `symlink_metadata`, so a symlink is skipped rather than followed off the root.
            let Ok(meta) = tokio::fs::symlink_metadata(e.path()).await else {
                continue;
            };
            if meta.is_dir() {
                stack.push((e.path(), key));
            } else if meta.is_file()
                && let Some(file) = entry(&key, meta.len())
            {
                files.push(file);
            }
        }
    }
    files.sort_by(|a, b| a.key.cmp(&b.key));
    files
}

/// One local file's bytes, or `None` when this machine holds no such capture.
pub async fn read_local(scratch_root: &Path, run_id: &str, key: &str) -> Option<Vec<u8>> {
    if !crate::runs::task_evidence::safe_segment(run_id) || !safe_key(key) {
        return None;
    }
    let root = crate::runs::task_evidence::files_root(scratch_root, run_id);
    let path = root.join(key);
    // The key is already component-checked, but resolve anyway: a symlinked capture directory
    // must not read outside the run's own root.
    let resolved = tokio::fs::canonicalize(&path).await.ok()?;
    let root = tokio::fs::canonicalize(&root).await.ok()?;
    if !resolved.starts_with(&root) {
        return None;
    }
    tokio::fs::read(&resolved).await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, *body)
                .expect("append");
        }
        let tar = builder.into_inner().expect("tar");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut enc, &tar).expect("gzip");
        enc.finish().expect("gzip")
    }

    #[test]
    fn a_listing_names_the_task_and_the_declared_path() {
        let gz = bundle(&[
            ("roundup/FINDINGS.json", b"{}"),
            ("propose/out/fix.patch", b"diff --git"),
        ]);
        let files = list_bundle(&gz).expect("list");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].task, "propose");
        assert_eq!(files[0].instance, None);
        assert_eq!(files[0].path, "out/fix.patch");
        assert_eq!(files[0].size_bytes, 10);
        assert_eq!(files[1].task, "roundup");
        assert_eq!(files[1].path, "FINDINGS.json");
    }

    #[test]
    fn sibling_fan_out_instances_stay_distinguishable() {
        let gz = bundle(&[
            ("audit[alpha]/REPORT.md", b"a"),
            ("audit[beta]/REPORT.md", b"bb"),
        ]);
        let files = list_bundle(&gz).expect("list");
        let keys: Vec<(&str, Option<&str>, &str)> = files
            .iter()
            .map(|f| (f.task.as_str(), f.instance.as_deref(), f.path.as_str()))
            .collect();
        assert_eq!(
            keys,
            [
                ("audit", Some("alpha"), "REPORT.md"),
                ("audit", Some("beta"), "REPORT.md"),
            ],
            "one task's instances must not collapse onto each other"
        );
        assert_eq!(
            read_bundle(&gz, "audit[beta]/REPORT.md").expect("read"),
            Some(b"bb".to_vec()),
            "each instance's content must be fetchable on its own key"
        );
    }

    #[test]
    fn a_declared_file_the_task_never_produced_is_absent() {
        // The engine publishes a task's capture directory by rename only once every declared file
        // is there, so a partial capture is a task with no directory at all.
        let gz = bundle(&[("roundup/FINDINGS.json", b"{}")]);
        let files = list_bundle(&gz).expect("list");
        assert!(
            files.iter().all(|f| f.task != "propose"),
            "a task that captured nothing must not appear"
        );
        assert_eq!(read_bundle(&gz, "propose/fix.patch").expect("read"), None);
    }

    /// A tar built byte by byte, because `tar::Builder` refuses to write a `..` path — which is
    /// exactly the entry a hostile or broken bundle would carry, and exactly what the reader has
    /// to survive.
    fn hostile_bundle(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = Vec::new();
        for (name, body) in entries {
            let mut header = [0u8; 512];
            header[..name.len()].copy_from_slice(name.as_bytes());
            header[100..108].copy_from_slice(b"0000644\0");
            header[108..116].copy_from_slice(b"0000000\0");
            header[116..124].copy_from_slice(b"0000000\0");
            header[124..136].copy_from_slice(format!("{:011o}\0", body.len()).as_bytes());
            header[136..148].copy_from_slice(b"00000000000\0");
            header[148..156].copy_from_slice(b"        ");
            header[156] = b'0';
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
            header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
            tar.extend_from_slice(&header);
            tar.extend_from_slice(body);
            tar.resize(tar.len().div_ceil(512) * 512, 0);
        }
        tar.extend_from_slice(&[0u8; 1024]);
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut enc, &tar).expect("gzip");
        enc.finish().expect("gzip")
    }

    #[test]
    fn a_traversing_entry_is_dropped_rather_than_served() {
        let gz = hostile_bundle(&[
            ("../../etc/passwd", b"root"),
            ("roundup/../../escape", b"no"),
            ("roundup/FINDINGS.json", b"{}"),
        ]);
        let files = list_bundle(&gz).expect("list");
        assert_eq!(
            files.iter().map(|f| f.key.as_str()).collect::<Vec<_>>(),
            ["roundup/FINDINGS.json"]
        );
        assert_eq!(read_bundle(&gz, "../../etc/passwd").expect("read"), None);
    }

    #[test]
    fn a_bare_file_outside_any_task_directory_is_not_a_capture() {
        let gz = bundle(&[("stray.txt", b"x")]);
        assert!(list_bundle(&gz).expect("list").is_empty());
    }

    /// A fan-out instance is keyed by the item it maps over, and docs-drift maps over document
    /// PATHS, so the instance's own name contains slashes. Splitting the key on its first `/`
    /// reported the task as `check[docs` and the path as `SUMMARY.md]/BROKEN.json`; a real run is
    /// what surfaced it, because every unit fixture until then used a slash-free instance key.
    #[test]
    fn an_instance_key_that_is_itself_a_path_keeps_the_task_and_the_declared_path_apart() {
        let gz = bundle(&[
            ("check[docs/SUMMARY.md]/BROKEN.json", b"[]"),
            ("check[docs/codex-harness.md]/CHECK.md", b"# x"),
        ]);
        let files = list_bundle(&gz).expect("list");
        let seen: Vec<(&str, Option<&str>, &str)> = files
            .iter()
            .map(|f| (f.task.as_str(), f.instance.as_deref(), f.path.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                ("check", Some("docs/SUMMARY.md"), "BROKEN.json"),
                ("check", Some("docs/codex-harness.md"), "CHECK.md"),
            ]
        );
        assert_eq!(
            read_bundle(&gz, "check[docs/SUMMARY.md]/BROKEN.json").expect("read"),
            Some(b"[]".to_vec())
        );
    }

    /// The engine escapes an instance's key where it forms the capture directory, so a key that is
    /// a path is one directory in the bundle. The listing reports the key the run named, and the
    /// file is served by the key the listing handed out.
    #[test]
    fn an_escaped_instance_directory_lists_its_raw_key() {
        let gz = bundle(&[
            ("check[docs%2FSUMMARY.md]/BROKEN.json", b"[]"),
            ("check[%2E.%2F..%2Fetc]/CHECK.md", b"# x"),
            ("check[50%25]/CHECK.md", b"# y"),
        ]);
        let files = list_bundle(&gz).expect("list");
        let seen: Vec<(&str, Option<&str>, &str, &str)> = files
            .iter()
            .map(|f| {
                (
                    f.task.as_str(),
                    f.instance.as_deref(),
                    f.path.as_str(),
                    f.key.as_str(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                (
                    "check",
                    Some("../../etc"),
                    "CHECK.md",
                    "check[%2E.%2F..%2Fetc]/CHECK.md"
                ),
                ("check", Some("50%"), "CHECK.md", "check[50%25]/CHECK.md"),
                (
                    "check",
                    Some("docs/SUMMARY.md"),
                    "BROKEN.json",
                    "check[docs%2FSUMMARY.md]/BROKEN.json"
                ),
            ]
        );
        assert_eq!(
            read_bundle(&gz, "check[docs%2FSUMMARY.md]/BROKEN.json").expect("read"),
            Some(b"[]".to_vec())
        );
    }

    #[test]
    fn an_instance_key_carrying_brackets_survives_the_split() {
        assert_eq!(
            split_instance("audit[a[0]]"),
            ("audit".to_string(), Some("a[0]".to_string()))
        );
        assert_eq!(split_instance("audit"), ("audit".to_string(), None));
    }

    #[tokio::test]
    async fn a_local_run_lists_and_serves_off_its_own_directory() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let root = crate::runs::task_evidence::files_root(scratch.path(), "run-1");
        tokio::fs::create_dir_all(root.join("roundup"))
            .await
            .expect("mkdir");
        tokio::fs::write(root.join("roundup/FINDINGS.json"), b"{\"n\":1}")
            .await
            .expect("write");

        let files = list_local(scratch.path(), "run-1").await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].key, "roundup/FINDINGS.json");
        assert_eq!(files[0].size_bytes, 7);
        assert_eq!(
            read_local(scratch.path(), "run-1", "roundup/FINDINGS.json").await,
            Some(b"{\"n\":1}".to_vec())
        );
        assert_eq!(
            read_local(scratch.path(), "run-1", "roundup/../../../etc/passwd").await,
            None
        );
    }

    /// One directory out of a pod run's bundle: its nested declared paths, nothing from a sibling
    /// task or a fan-out instance of the same name, and no bytes for a file over the inline cap.
    #[tokio::test]
    async fn a_bundles_directory_holds_only_that_directorys_files() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let gz = bundle(&[
            ("collect/evidence/probe.json", b"{}"),
            ("collect/HEALTH.json", b"0123456789"),
            ("collector/OTHER.md", b"x"),
            ("collect[a]/REPORT.md", b"y"),
            ("assemble/REPORT.md", b"z"),
        ]);
        let files = dir_files(Some(&gz), scratch.path(), "run-1", "collect", 4)
            .await
            .expect("read");
        let seen: Vec<(&str, u64, Option<&[u8]>)> = files
            .iter()
            .map(|(f, body)| (f.path.as_str(), f.size_bytes, body.as_deref()))
            .collect();
        assert_eq!(
            seen,
            [
                ("HEALTH.json", 10, None),
                ("evidence/probe.json", 2, Some(&b"{}"[..])),
            ]
        );

        let instance = dir_files(Some(&gz), scratch.path(), "run-1", "collect[a]", 1024)
            .await
            .expect("read");
        assert_eq!(instance.len(), 1);
        assert_eq!(instance[0].0.instance.as_deref(), Some("a"));
        assert_eq!(instance[0].1.as_deref(), Some(&b"y"[..]));
    }

    /// A stored bundle is the run's files, whatever this machine also happens to hold.
    #[tokio::test]
    async fn a_stored_bundle_wins_over_the_scratch_disk() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let root = crate::runs::task_evidence::files_root(scratch.path(), "run-1");
        tokio::fs::create_dir_all(root.join("collect"))
            .await
            .expect("mkdir");
        tokio::fs::write(root.join("collect/STALE.md"), b"local")
            .await
            .expect("write");
        let gz = bundle(&[("collect/HEALTH.json", b"{}")]);

        let stored = dir_files(Some(&gz), scratch.path(), "run-1", "collect", 1024)
            .await
            .expect("read");
        assert_eq!(
            stored
                .iter()
                .map(|(f, _)| f.key.as_str())
                .collect::<Vec<_>>(),
            ["collect/HEALTH.json"]
        );
        let local = dir_files(None, scratch.path(), "run-1", "collect", 1024)
            .await
            .expect("read");
        assert_eq!(
            local
                .iter()
                .map(|(f, _)| f.key.as_str())
                .collect::<Vec<_>>(),
            ["collect/STALE.md"]
        );
    }

    #[tokio::test]
    async fn a_symlinked_capture_reads_nothing() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let root = crate::runs::task_evidence::files_root(scratch.path(), "run-1");
        tokio::fs::create_dir_all(root.join("roundup"))
            .await
            .expect("mkdir");
        let secret = scratch.path().join("secret");
        tokio::fs::write(&secret, b"nope").await.expect("write");
        std::os::unix::fs::symlink(&secret, root.join("roundup/LEAK.md")).expect("symlink");

        assert!(
            list_local(scratch.path(), "run-1").await.is_empty(),
            "a symlink is not a captured file"
        );
        assert_eq!(
            read_local(scratch.path(), "run-1", "roundup/LEAK.md").await,
            None
        );
    }

    #[tokio::test]
    async fn files_are_read_under_the_runs_own_directory_only() {
        let root = tempfile::tempdir().expect("tempdir");
        let scratch = root.path();
        let task_dir =
            crate::runs::task_evidence::files_root(scratch, "run-1").join("triage[1027]");
        std::fs::create_dir_all(&task_dir).expect("mkdir");
        std::fs::write(task_dir.join("TRIAGE.md"), "# triage\n").expect("write");
        std::fs::write(task_dir.join("blob.bin"), [0xff_u8, 0xfe, 0x00]).expect("write");
        std::fs::create_dir(task_dir.join("nested")).expect("mkdir");

        let files = captured_files(None, scratch, "run-1", "triage[1027]")
            .await
            .expect("read");
        assert_eq!(files.len(), 2, "the directory is not a file: {files:?}");
        assert_eq!(files[0].name, "TRIAGE.md");
        assert_eq!(files[0].content.as_deref(), Some("# triage\n"));
        assert_eq!(files[1].name, "blob.bin");
        assert_eq!(files[1].content, None, "not decodable as text");

        // A traversal in either segment reads nothing, however the join would have resolved.
        assert!(
            captured_files(None, scratch, "run-1", "..")
                .await
                .expect("read")
                .is_empty()
        );
        assert!(
            captured_files(None, scratch, "..", "triage[1027]")
                .await
                .expect("read")
                .is_empty()
        );
        assert!(
            captured_files(None, scratch, "run-1", "../../../../etc")
                .await
                .expect("read")
                .is_empty()
        );
    }

    /// An instance named after a key holding a separator captured into the engine's escaped
    /// directory, and its evidence is read from there by the name the session log carries.
    #[tokio::test]
    async fn an_instance_whose_key_holds_a_separator_reads_its_escaped_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let scratch = root.path();
        let task_dir = crate::runs::task_evidence::files_root(scratch, "run-1")
            .join("audit[llm-d%2Fllm-d-router]");
        std::fs::create_dir_all(&task_dir).expect("mkdir");
        std::fs::write(task_dir.join("out.txt"), "router\n").expect("write");

        let files = captured_files(None, scratch, "run-1", "audit[llm-d/llm-d-router]")
            .await
            .expect("read");
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(files[0].content.as_deref(), Some("router\n"));
        assert!(
            captured_files(None, scratch, "run-1", "audit[llm-d%2Fllm-d-router]")
                .await
                .expect("read")
                .is_empty(),
            "the escaped text is not itself the raw key"
        );
    }

    #[tokio::test]
    async fn a_symlinked_task_directory_reads_nothing() {
        let root = tempfile::tempdir().expect("tempdir");
        let scratch = root.path();
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(outside.join("SECRET.md"), "not yours\n").expect("write");
        let files = crate::runs::task_evidence::files_root(scratch, "run-1");
        std::fs::create_dir_all(&files).expect("mkdir");
        std::os::unix::fs::symlink(&outside, files.join("escape")).expect("symlink");

        assert!(
            captured_files(None, scratch, "run-1", "escape")
                .await
                .expect("read")
                .is_empty()
        );
    }

    /// A task may declare a file under a subdirectory, and it is listed by the path it declared.
    #[tokio::test]
    async fn a_declared_file_under_a_subdirectory_is_listed_by_its_declared_path() {
        let root = tempfile::tempdir().expect("tempdir");
        let scratch = root.path();
        let task_dir = crate::runs::task_evidence::files_root(scratch, "run-1").join("collect");
        std::fs::create_dir_all(task_dir.join("evidence")).expect("mkdir");
        std::fs::write(task_dir.join("evidence/probe.json"), "{}").expect("write");
        std::fs::write(task_dir.join("REPORT.md"), "# r\n").expect("write");

        let files = captured_files(None, scratch, "run-1", "collect")
            .await
            .expect("read");
        let names: Vec<(&str, Option<&str>)> = files
            .iter()
            .map(|f| (f.name.as_str(), f.content.as_deref()))
            .collect();
        assert_eq!(
            names,
            [
                ("REPORT.md", Some("# r\n")),
                ("evidence/probe.json", Some("{}"))
            ]
        );
    }
}
