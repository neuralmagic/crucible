//! Pure filesystem-diff <-> OCI-layer plumbing: turn a set of changed entries into a valid OCI layer
//! tar (added/modified files, whiteouts for deletions, opaque-dir markers) and, inversely, apply a
//! layer tar over a directory tree honoring those whiteout conventions. Dir-in / tar-out and
//! tar-in / dir-out, with no knowledge of overlayfs, chroot, or registries, so the correctness of the
//! diff format can be nailed down in isolation.
//!
//! OCI whiteout conventions (image-spec layer format):
//!   - a deleted `dir/name`   -> an empty regular file `dir/.wh.name`
//!   - a dir whose lower contents are hidden -> an empty regular file `dir/.wh..wh..opaque` in it

use anyhow::{Context, Result};
use flate2::Compression;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

/// The OCI whiteout prefix marking a deleted entry, and the exact filename marking an opaque dir.
const WH_PREFIX: &str = ".wh.";
const WH_OPAQUE: &str = ".wh..wh..opaque";

/// A tar member whose path escapes the extraction root. Refused rather than normalized: a
/// layer that ships `../` is not something to guess the intent of.
#[derive(Debug, thiserror::Error)]
#[error("unsafe tar member path {}", .path.display())]
pub struct UnsafeTarMemberPath {
    path: std::path::PathBuf,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// One change relative to the base rootfs, at an in-image path with no leading slash (e.g.
/// `usr/local/lib/python3.12/site-packages/foo.pth`). The closed set an overlayfs upperdir can
/// produce; serialized to a layer tar by [`build_layer_tar`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerEntry {
    /// A directory present in the diff. `opaque` emits the `.wh..wh..opaque` marker so the base's
    /// contents at this path are hidden (an overlayfs "opaque" dir).
    Dir {
        path: String,
        mode: u32,
        uid: u32,
        gid: u32,
        opaque: bool,
    },
    /// A regular file, added or modified. `mode` carries the permission bits (the executable bit lives here).
    File {
        path: String,
        mode: u32,
        uid: u32,
        gid: u32,
        contents: Vec<u8>,
    },
    /// A symbolic link at `path` pointing at `target`.
    Symlink {
        path: String,
        target: String,
        uid: u32,
        gid: u32,
    },
    /// A deletion: the base had `path`, the diff removes it (emitted as a `.wh.` marker).
    Whiteout { path: String },
}

/// A built layer: the gzipped tar to upload plus the two digests an image references it by, the
/// compressed blob digest (named in the manifest) and the uncompressed diff_id (named in the config
/// rootfs). Deterministic: the same entries yield the same digests.
pub(crate) struct BuiltLayer {
    pub(crate) gzip: Vec<u8>,
    pub(crate) blob_digest: String,
    pub(crate) diff_id: String,
    pub(crate) size: i64,
}

impl BuiltLayer {
    /// The uncompressed diff_id (`sha256:…`), as named in the config `rootfs.diff_ids`.
    pub(crate) fn diff_id(&self) -> &str {
        &self.diff_id
    }

    /// Gzip an uncompressed layer tar deterministically (fixed gzip header mtime) and compute both
    /// digests. Splitting build (tar) from compress (this) keeps the tar bytes available for the
    /// diff_id while the gzip drives the blob digest.
    pub(crate) fn from_tar(tar_bytes: &[u8]) -> Result<Self> {
        let diff_id = format!("sha256:{}", sha256_hex(tar_bytes));
        let mut enc = flate2::GzBuilder::new().write(Vec::new(), Compression::default());
        enc.write_all(tar_bytes).context("gzipping layer tar")?;
        let gzip = enc.finish().context("finishing layer gzip")?;
        let blob_digest = format!("sha256:{}", sha256_hex(&gzip));
        let size = gzip.len() as i64;
        Ok(Self {
            gzip,
            blob_digest,
            diff_id,
            size,
        })
    }
}

/// Split an in-image path into its parent dir (possibly empty) and final component.
fn split_parent(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    }
}

/// Serialize `entries` into an uncompressed OCI layer tar. Deterministic (mtime zeroed). Entries must
/// be ordered parent-before-child so a consumer that clears an opaque dir sees the marker before the
/// dir's own new contents. Permissions/ownership are preserved verbatim from each entry.
pub(crate) fn build_layer_tar(entries: &[LayerEntry]) -> Result<Vec<u8>> {
    let mut tar_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        for entry in entries {
            match entry {
                LayerEntry::Dir {
                    path,
                    mode,
                    uid,
                    gid,
                    opaque,
                } => {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_size(0);
                    header.set_mode(*mode);
                    header.set_uid(*uid as u64);
                    header.set_gid(*gid as u64);
                    header.set_mtime(0);
                    let dir_path = format!("{}/", path.trim_end_matches('/'));
                    builder
                        .append_data(&mut header, &dir_path, std::io::empty())
                        .with_context(|| format!("tarring dir {path}"))?;
                    if *opaque {
                        append_marker(
                            &mut builder,
                            &format!("{}/{WH_OPAQUE}", path.trim_end_matches('/')),
                        )?;
                    }
                }
                LayerEntry::File {
                    path,
                    mode,
                    uid,
                    gid,
                    contents,
                } => {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_size(contents.len() as u64);
                    header.set_mode(*mode);
                    header.set_uid(*uid as u64);
                    header.set_gid(*gid as u64);
                    header.set_mtime(0);
                    builder
                        .append_data(&mut header, path, contents.as_slice())
                        .with_context(|| format!("tarring file {path}"))?;
                }
                LayerEntry::Symlink {
                    path,
                    target,
                    uid,
                    gid,
                } => {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_size(0);
                    header.set_mode(0o777);
                    header.set_uid(*uid as u64);
                    header.set_gid(*gid as u64);
                    header.set_mtime(0);
                    builder
                        .append_link(&mut header, path, Path::new(target))
                        .with_context(|| format!("tarring symlink {path} -> {target}"))?;
                }
                LayerEntry::Whiteout { path } => {
                    let (parent, name) = split_parent(path);
                    let wh = if parent.is_empty() {
                        format!("{WH_PREFIX}{name}")
                    } else {
                        format!("{parent}/{WH_PREFIX}{name}")
                    };
                    append_marker(&mut builder, &wh)?;
                }
            }
        }
        builder.finish().context("finishing layer tar")?;
    }
    Ok(tar_buf)
}

/// Append a zero-length marker file (a whiteout or opaque marker) at `path`.
fn append_marker<W: Write>(builder: &mut tar::Builder<W>, path: &str) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(0);
    header.set_mode(0o644);
    header.set_mtime(0);
    builder
        .append_data(&mut header, path, std::io::empty())
        .with_context(|| format!("tarring marker {path}"))?;
    Ok(())
}

/// Reject a tar member path that would escape `dest` (absolute, or containing `..`). A layer from a
/// registry is untrusted input; a `../../etc/passwd` member must never write outside the rootfs.
fn safe_join(dest: &Path, rel: &Path) -> Result<PathBuf> {
    let mut out = dest.to_path_buf();
    for comp in rel.components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                return Err(UnsafeTarMemberPath {
                    path: rel.to_path_buf(),
                }
                .into());
            }
        }
    }
    Ok(out)
}

/// Best-effort ownership set on an extracted path (needs privilege; a no-op failure keeps unprivileged
/// unpacking working, and the loop pod runs privileged where it matters). `lchown` so a symlink's own
/// ownership is set, never its target's.
fn lchown_best_effort(path: &Path, uid: u32, gid: u32) {
    use std::os::unix::ffi::OsStrExt;
    if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
        // SAFETY: `c` is a valid NUL-terminated path; lchown only reads it and the two ids.
        unsafe {
            libc::lchown(c.as_ptr(), uid, gid);
        }
    }
}

/// Apply an OCI layer tar (`reader`, uncompressed) over the tree at `dest`, honoring whiteouts: a
/// `.wh.name` deletes `name`, a `.wh..wh..opaque` clears the enclosing dir's existing contents. Used
/// both to stack base-image layers into a rootfs and, in tests, to verify a built layer round-trips.
/// Ownership is applied best-effort (see [`lchown_best_effort`]).
pub(crate) fn apply_layer_reader<R: Read>(reader: R, dest: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().context("reading layer tar entries")? {
        let mut entry = entry.context("reading a layer tar entry")?;
        let rel = entry
            .path()
            .context("layer entry has no path")?
            .into_owned();
        let (parent, name) = match (rel.parent(), rel.file_name()) {
            (Some(p), Some(n)) => (p.to_path_buf(), n.to_string_lossy().into_owned()),
            _ => continue,
        };

        if name == WH_OPAQUE {
            let dir = safe_join(dest, &parent)?;
            if dir.is_dir() {
                for child in std::fs::read_dir(&dir).with_context(|| format!("reading {dir:?}"))? {
                    let child = child?.path();
                    remove_any(&child)?;
                }
            }
            continue;
        }
        if let Some(stripped) = name.strip_prefix(WH_PREFIX) {
            let target = safe_join(dest, &parent)?.join(stripped);
            remove_any(&target)?;
            continue;
        }

        let target = safe_join(dest, &rel)?;
        let header = entry.header().clone();
        let mode = header.mode().unwrap_or(0o644) & 0o7777;
        let uid = header.uid().unwrap_or(0) as u32;
        let gid = header.gid().unwrap_or(0) as u32;
        match header.entry_type() {
            tar::EntryType::Directory => {
                std::fs::create_dir_all(&target).with_context(|| format!("mkdir {target:?}"))?;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                    .with_context(|| format!("chmod {target:?}"))?;
                lchown_best_effort(&target, uid, gid);
            }
            tar::EntryType::Symlink => {
                let link = header
                    .link_name()
                    .context("symlink entry has no target")?
                    .context("symlink entry has no target")?
                    .into_owned();
                if let Some(p) = target.parent() {
                    std::fs::create_dir_all(p).with_context(|| format!("mkdir {p:?}"))?;
                }
                remove_any(&target)?;
                std::os::unix::fs::symlink(&link, &target)
                    .with_context(|| format!("symlink {target:?} -> {link:?}"))?;
                lchown_best_effort(&target, uid, gid);
            }
            tar::EntryType::Regular => {
                if let Some(p) = target.parent() {
                    std::fs::create_dir_all(p).with_context(|| format!("mkdir {p:?}"))?;
                }
                remove_any(&target)?;
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf).context("reading file bytes")?;
                std::fs::write(&target, &buf).with_context(|| format!("writing {target:?}"))?;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                    .with_context(|| format!("chmod {target:?}"))?;
                lchown_best_effort(&target, uid, gid);
            }
            // Hardlinks are how busybox-style images ship their applets (`/bin/sh` -> busybox):
            // materialize them against the already-extracted target (tar lists the target first).
            tar::EntryType::Link => {
                let link = header
                    .link_name()
                    .context("hardlink entry has no target")?
                    .context("hardlink entry has no target")?
                    .into_owned();
                let src = safe_join(dest, &link)?;
                if let Some(p) = target.parent() {
                    std::fs::create_dir_all(p).with_context(|| format!("mkdir {p:?}"))?;
                }
                remove_any(&target)?;
                std::fs::hard_link(&src, &target)
                    .with_context(|| format!("hardlink {target:?} -> {src:?}"))?;
            }
            // char/block/fifo: rare in these rootfs layers; skip rather than guess.
            _ => {}
        }
    }
    Ok(())
}

/// Remove a path whether it's a file, symlink, or directory; a missing path is not an error.
fn remove_any(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            std::fs::remove_dir_all(path).with_context(|| format!("rm -r {path:?}"))
        }
        Ok(_) => std::fs::remove_file(path).with_context(|| format!("rm {path:?}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("stat {path:?}")),
    }
}

/// A comparable snapshot of a tree's shape (relative paths, kinds, perms, symlink targets, file
/// contents), skipping uid/gid so it round-trips under an unprivileged test. For asserting that
/// applying a built layer over a lower copy reproduces the intended tree.
#[cfg(test)]
fn tree_snapshot(root: &Path) -> std::collections::BTreeSet<String> {
    fn walk(root: &Path, dir: &Path, out: &mut std::collections::BTreeSet<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .into_owned();
            let meta = match std::fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.file_type().is_symlink() {
                let tgt = std::fs::read_link(&p).unwrap_or_default();
                out.insert(format!("L {rel} -> {}", tgt.display()));
            } else if meta.is_dir() {
                out.insert(format!("D {rel} {:o}", meta.permissions().mode() & 0o777));
                walk(root, &p, out);
            } else {
                let bytes = std::fs::read(&p).unwrap_or_default();
                out.insert(format!(
                    "F {rel} {:o} {}",
                    meta.permissions().mode() & 0o777,
                    sha256_hex(&bytes)
                ));
            }
        }
    }
    let mut out = std::collections::BTreeSet::new();
    walk(root, root, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect the (path, entry_type, mode, link) tuples a tar holds, for exact-shape assertions.
    fn tar_members(tar_bytes: &[u8]) -> Vec<(String, tar::EntryType, u32, Option<String>)> {
        let mut ar = tar::Archive::new(tar_bytes);
        let mut out = Vec::new();
        for e in ar.entries().unwrap() {
            let e = e.unwrap();
            let h = e.header();
            let path = e.path().unwrap().to_string_lossy().into_owned();
            let link = h
                .link_name()
                .ok()
                .flatten()
                .map(|l| l.to_string_lossy().into_owned());
            out.push((path, h.entry_type(), h.mode().unwrap() & 0o7777, link));
        }
        out
    }

    fn read_file_from_tar(tar_bytes: &[u8], want: &str) -> Option<Vec<u8>> {
        let mut ar = tar::Archive::new(tar_bytes);
        for e in ar.entries().unwrap() {
            let mut e = e.unwrap();
            if e.path().unwrap().to_string_lossy() == want {
                let mut v = Vec::new();
                e.read_to_end(&mut v).unwrap();
                return Some(v);
            }
        }
        None
    }

    #[test]
    fn tar_holds_exact_entries_for_each_kind() {
        let entries = vec![
            LayerEntry::Dir {
                path: "opt/app".into(),
                mode: 0o755,
                uid: 0,
                gid: 0,
                opaque: false,
            },
            LayerEntry::File {
                path: "opt/app/run.sh".into(),
                mode: 0o755,
                uid: 0,
                gid: 0,
                contents: b"#!/bin/sh\necho hi\n".to_vec(),
            },
            LayerEntry::File {
                path: "opt/app/data.txt".into(),
                mode: 0o644,
                uid: 1000,
                gid: 1000,
                contents: b"payload".to_vec(),
            },
            LayerEntry::Symlink {
                path: "opt/app/link".into(),
                target: "run.sh".into(),
                uid: 0,
                gid: 0,
            },
            LayerEntry::Whiteout {
                path: "opt/app/gone".into(),
            },
        ];
        let tar = build_layer_tar(&entries).unwrap();
        let members = tar_members(&tar);

        // The directory has a trailing slash and its mode.
        assert!(
            members.iter().any(|(p, t, m, _)| p == "opt/app/"
                && *t == tar::EntryType::Directory
                && *m == 0o755)
        );
        // The executable bit survives on run.sh.
        assert!(members.iter().any(|(p, t, m, _)| p == "opt/app/run.sh"
            && *t == tar::EntryType::Regular
            && *m == 0o755));
        assert!(
            members
                .iter()
                .any(|(p, _, m, _)| p == "opt/app/data.txt" && *m == 0o644)
        );
        // The symlink is a symlink pointing at its target.
        assert!(members.iter().any(|(p, t, _, l)| p == "opt/app/link"
            && *t == tar::EntryType::Symlink
            && l.as_deref() == Some("run.sh")));
        // The deletion is a zero-length .wh. marker in the parent dir.
        assert!(
            members
                .iter()
                .any(|(p, t, _, _)| p == "opt/app/.wh.gone" && *t == tar::EntryType::Regular)
        );

        assert_eq!(
            read_file_from_tar(&tar, "opt/app/data.txt").as_deref(),
            Some(&b"payload"[..])
        );
    }

    #[test]
    fn opaque_dir_emits_the_opaque_marker() {
        let entries = vec![LayerEntry::Dir {
            path: "var/cache".into(),
            mode: 0o755,
            uid: 0,
            gid: 0,
            opaque: true,
        }];
        let tar = build_layer_tar(&entries).unwrap();
        let members = tar_members(&tar);
        assert!(members.iter().any(|(p, _, _, _)| p == "var/cache/"));
        assert!(
            members
                .iter()
                .any(|(p, t, _, _)| p == "var/cache/.wh..wh..opaque"
                    && *t == tar::EntryType::Regular)
        );
    }

    #[test]
    fn top_level_whiteout_has_no_parent_dir_in_the_path() {
        let entries = vec![LayerEntry::Whiteout {
            path: "toplevel".into(),
        }];
        let tar = build_layer_tar(&entries).unwrap();
        let members = tar_members(&tar);
        assert!(members.iter().any(|(p, _, _, _)| p == ".wh.toplevel"));
    }

    #[test]
    fn built_layer_is_deterministic_and_has_distinct_digests() {
        let entries = vec![LayerEntry::File {
            path: "a/b.txt".into(),
            mode: 0o644,
            uid: 0,
            gid: 0,
            contents: b"x".to_vec(),
        }];
        let tar = build_layer_tar(&entries).unwrap();
        let a = BuiltLayer::from_tar(&tar).unwrap();
        let b = BuiltLayer::from_tar(&tar).unwrap();
        assert_eq!(a.blob_digest, b.blob_digest);
        assert!(a.blob_digest.starts_with("sha256:"));
        assert!(a.diff_id.starts_with("sha256:"));
        // Compressed blob digest and uncompressed diff_id are necessarily different.
        assert_ne!(a.blob_digest, a.diff_id);
        assert_eq!(a.size as usize, a.gzip.len());
    }

    /// Round-trip: build a layer of adds/modifies/deletes/opaque over a synthetic lower dir, apply it,
    /// and assert the resulting tree matches the intended union. This is the whiteout semantics test.
    #[test]
    fn layer_applied_over_lower_reproduces_the_intended_tree() {
        let base = std::env::temp_dir().join(format!("forge-layer-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let lower = base.join("lower");

        // Lower: keep.txt, replace.txt, del.txt, and hidden/ with a lower file to be opaqued away.
        std::fs::create_dir_all(lower.join("hidden")).unwrap();
        std::fs::write(lower.join("keep.txt"), b"keep").unwrap();
        std::fs::write(lower.join("replace.txt"), b"old").unwrap();
        std::fs::write(lower.join("del.txt"), b"bye").unwrap();
        std::fs::write(lower.join("hidden/lower-only"), b"should vanish").unwrap();

        let entries = vec![
            LayerEntry::File {
                path: "replace.txt".into(),
                mode: 0o644,
                uid: 0,
                gid: 0,
                contents: b"new".to_vec(),
            },
            LayerEntry::File {
                path: "added.sh".into(),
                mode: 0o755,
                uid: 0,
                gid: 0,
                contents: b"#!/bin/sh\n".to_vec(),
            },
            LayerEntry::Whiteout {
                path: "del.txt".into(),
            },
            LayerEntry::Dir {
                path: "hidden".into(),
                mode: 0o755,
                uid: 0,
                gid: 0,
                opaque: true,
            },
            LayerEntry::File {
                path: "hidden/upper-only".into(),
                mode: 0o644,
                uid: 0,
                gid: 0,
                contents: b"survives".to_vec(),
            },
        ];
        let tar = build_layer_tar(&entries).unwrap();
        apply_layer_reader(tar.as_slice(), &lower).unwrap();

        let snap = tree_snapshot(&lower);
        let has = |prefix: &str| snap.iter().any(|s| s.starts_with(prefix));
        // Untouched lower file stays.
        assert!(has("F keep.txt"));
        // Deleted file is gone.
        assert!(!has("F del.txt"));
        // Replaced file has the new contents.
        assert_eq!(std::fs::read(lower.join("replace.txt")).unwrap(), b"new");
        // Added executable exists with its bit.
        assert!(snap.iter().any(|s| s.starts_with("F added.sh 755")));
        // Opaque dir hid the lower-only file but kept the upper-only one.
        assert!(!has("F hidden/lower-only"));
        assert_eq!(
            std::fs::read(lower.join("hidden/upper-only")).unwrap(),
            b"survives"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn apply_materializes_hardlinks_against_the_extracted_target() {
        // busybox-style layer: one real binary + an applet hardlinked to it.
        let mut tar_buf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_buf);
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(7);
            h.set_mode(0o755);
            h.set_mtime(0);
            b.append_data(&mut h, "bin/busybox", &b"BINARY\n"[..])
                .unwrap();
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Link);
            h.set_size(0);
            h.set_mode(0o755);
            h.set_mtime(0);
            b.append_link(&mut h, "bin/sh", "bin/busybox").unwrap();
            b.finish().unwrap();
        }
        let dest = std::env::temp_dir().join(format!("forge-hardlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::create_dir_all(&dest).unwrap();
        apply_layer_reader(tar_buf.as_slice(), &dest).unwrap();

        assert_eq!(std::fs::read(dest.join("bin/sh")).unwrap(), b"BINARY\n");
        // A real hardlink: same inode as the target.
        use std::os::unix::fs::MetadataExt;
        let a = std::fs::metadata(dest.join("bin/busybox")).unwrap();
        let b = std::fs::metadata(dest.join("bin/sh")).unwrap();
        assert_eq!(a.ino(), b.ino());

        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn safe_join_refuses_paths_that_escape_dest() {
        // A registry layer is untrusted: a `..` or absolute member must never resolve outside dest.
        let dest = Path::new("/rootfs");
        assert!(safe_join(dest, Path::new("usr/bin/tool")).is_ok());
        assert!(safe_join(dest, Path::new("./a/./b")).is_ok());
        assert!(safe_join(dest, Path::new("../escape")).is_err());
        assert!(safe_join(dest, Path::new("a/../../escape")).is_err());
        assert!(safe_join(dest, Path::new("/etc/passwd")).is_err());
    }
}
