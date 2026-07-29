//! Layer capture: run a command against a base image's own root filesystem and turn the files it wrote
//! into a pushable OCI layer, with no container runtime and no full image rebuild. The fast path for
//! "install this source tree into the image" candidates, where a buildah bud re-pulls a ~20GB base into
//! its own storage every time.
//!
//! ```text
//!   base image (by digest) ──unpack once──▶ cached rootfs (lowerdir, reused, LRU-evicted)
//!                                                 │  overlayfs: lower=rootfs, upper=fresh
//!                                                 ▼
//!             run `cmd` chrooted into the merged view (image's own python/env, source bind-mounted)
//!                                                 │  writes land in upper
//!                                                 ▼
//!             scan upper ──▶ LayerEntry diff ──▶ OCI layer tar ──▶ append to base + push ──▶ ref@sha256
//! ```
//!
//! The pieces around execution (rootfs cache, upperdir scan, diff->layer, append+push) are
//! mechanism-agnostic and unit-tested; the environment-specific mount+chroot step lives behind the
//! [`Executor`] trait. The default executor is unprivileged ([`UsernsExecutor`]: a self-mapped user
//! namespace that mounts overlayfs inside it); the direct privileged mount ([`OverlayfsChroot`]) is
//! selected only when probing shows it actually works, or when explicitly configured.

use crate::layer::{BuiltLayer, LayerEntry, build_layer_tar};
use anyhow::{Context, Result, bail};
use oci_client::secrets::RegistryAuth;
#[cfg(target_os = "linux")]
use std::ffi::{CStr, CString};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::runtime::Handle;

/// A directory bind-mounted into the execution root, e.g. a checked-out source tree at
/// `/workspace/src` so the command can `pip install -e /workspace/src`.
#[derive(Debug, Clone)]
pub struct BindMount {
    pub host: PathBuf,
    pub container: String,
}

/// The unpacked, digest-keyed base root filesystem used as the overlay lowerdir. Opaque handle so a
/// caller can't accidentally treat an arbitrary path as a prepared rootfs.
#[derive(Debug, Clone)]
pub struct Rootfs {
    dir: PathBuf,
}

impl Rootfs {
    fn path(&self) -> &Path {
        &self.dir
    }
}

/// Keep this many unpacked rootfs (newest-used first) before evicting; at ~20GB per base that bounds
/// the cache around 60GB by default.
const DEFAULT_ROOTFS_KEEP: usize = 3;
/// Never evict a rootfs used this recently: a concurrent capture may still be running on it.
const EVICT_MIN_IDLE: Duration = Duration::from_secs(6 * 3600);
/// Staging/scratch dirs older than this are crash leftovers, safe to sweep.
const STALE_SCRATCH_AGE: Duration = Duration::from_secs(24 * 3600);

/// A content-addressed cache of unpacked base root filesystems, rooted at a directory on the loop-pod
/// volume. Each base is unpacked once, keyed by its resolved linux/amd64 manifest digest, reused (a hit
/// refreshes its LRU stamp), and evicted least-recently-used beyond [`DEFAULT_ROOTFS_KEEP`]. All registry
/// IO runs on the shared runtime `handle` taken at construction.
#[derive(Debug, Clone)]
pub struct RootfsCache {
    root: PathBuf,
    handle: Handle,
    keep: usize,
}

/// The rootfs dir and its completion marker for a given digest under `root`. Pure (no I/O) so the
/// keying can be tested without a network pull. The marker is a sibling of the rootfs dir, not inside
/// it, so the rootfs stays a pristine image root; its mtime doubles as the LRU stamp.
fn rootfs_paths(root: &Path, digest: &str) -> (PathBuf, PathBuf) {
    let safe = digest.replace([':', '/'], "_");
    let dir = root.join("rootfs").join(&safe);
    let marker = root.join("rootfs").join(format!("{safe}.done"));
    (dir, marker)
}

/// Which cache entries to evict: everything past the `keep` most-recently-used, except entries used
/// within `min_idle` (a concurrent capture may still be on them). Pure, so the policy is testable.
fn eviction_victims(
    entries: &[(String, SystemTime)],
    keep: usize,
    now: SystemTime,
    min_idle: Duration,
) -> Vec<String> {
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
    sorted
        .into_iter()
        .skip(keep)
        .filter(|(_, t)| {
            now.duration_since(*t)
                .map(|idle| idle >= min_idle)
                .unwrap_or(false)
        })
        .map(|(name, _)| name)
        .collect()
}

impl RootfsCache {
    pub fn new(root: impl Into<PathBuf>, handle: Handle) -> Self {
        Self {
            root: root.into(),
            handle,
            keep: DEFAULT_ROOTFS_KEEP,
        }
    }

    /// Override how many unpacked rootfs to keep before LRU eviction (minimum 1).
    pub fn with_keep(mut self, keep: usize) -> Self {
        self.keep = keep.max(1);
        self
    }

    fn runtime(&self) -> &Handle {
        &self.handle
    }

    /// Return the cached rootfs for `base_ref`, unpacking it once if absent. Resolving the amd64 digest
    /// is a cheap manifest fetch; only a cache miss pulls the (large) layer blobs. The unpack lands in a
    /// staging dir and is atomically renamed into place, so a crash never leaves a half-rootfs marked done.
    fn ensure(&self, base_ref: &str, auth: &RegistryAuth) -> Result<Rootfs> {
        let digest = self
            .handle
            .block_on(crate::oci::resolve_platform_digest(base_ref, auth))?;
        let (dir, marker) = rootfs_paths(&self.root, &digest);
        if marker.exists() && dir.is_dir() {
            // Rewrite the marker so its mtime records this use (the LRU stamp).
            let _ = std::fs::write(&marker, digest.as_bytes());
            tracing::info!(base = base_ref, digest, "rootfs cache hit");
            return Ok(Rootfs { dir });
        }
        tracing::info!(base = base_ref, digest, "rootfs cache miss; unpacking");
        let started = SystemTime::now();

        let parent = dir
            .parent()
            .context("rootfs cache dir has no parent")?
            .to_path_buf();
        std::fs::create_dir_all(&parent)
            .with_context(|| format!("creating rootfs cache root {parent:?}"))?;
        let staging = parent.join(format!(
            ".staging-{}-{}",
            std::process::id(),
            digest.replace([':', '/'], "_")
        ));
        let _ = std::fs::remove_dir_all(&staging);

        self.handle
            .block_on(crate::oci::unpack_rootfs(base_ref, &staging, auth))
            .with_context(|| format!("unpacking rootfs for {base_ref}"))?;

        // A concurrent capture may have finished the same digest first; if so, drop ours and use theirs.
        if !(marker.exists() && dir.is_dir()) {
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::rename(&staging, &dir)
                .with_context(|| format!("promoting staged rootfs into {dir:?}"))?;
            std::fs::write(&marker, digest.as_bytes())
                .with_context(|| format!("writing rootfs marker {marker:?}"))?;
        } else {
            let _ = std::fs::remove_dir_all(&staging);
        }
        tracing::info!(
            base = base_ref,
            secs = started.elapsed().map(|d| d.as_secs()).unwrap_or(0),
            "rootfs unpacked into the cache"
        );
        self.evict();
        Ok(Rootfs { dir })
    }

    /// Evict least-recently-used rootfs beyond the keep budget and sweep crash-leftover staging dirs.
    /// Best-effort: an eviction hiccup must never fail the capture that triggered it. Marker first, dir
    /// second, so a half-removed entry reads as absent (no marker), never as valid.
    fn evict(&self) {
        let dir = self.root.join("rootfs");
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return;
        };
        let now = SystemTime::now();
        let mut entries = Vec::new();
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let mtime = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if let Some(stem) = name.strip_suffix(".done") {
                entries.push((stem.to_string(), mtime));
            } else if name.starts_with(".staging-")
                && now
                    .duration_since(mtime)
                    .map(|age| age >= STALE_SCRATCH_AGE)
                    .unwrap_or(false)
            {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        for stem in eviction_victims(&entries, self.keep, now, EVICT_MIN_IDLE) {
            tracing::info!(rootfs = stem, "evicting least-recently-used rootfs");
            let _ = std::fs::remove_file(dir.join(format!("{stem}.done")));
            let _ = std::fs::remove_dir_all(dir.join(&stem));
        }
    }
}

/// The prepared overlay directories and the command context for one capture. `lower` is the cached
/// rootfs; `upper` receives the diff; `work` is overlayfs's scratch; `merged` is the mountpoint the
/// command runs chrooted into. All four are on the same filesystem (an overlayfs requirement).
// The fields are only read by the linux overlayfs/chroot executor; on other hosts the struct is
// plumbing that never executes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct ExecPlan<'a> {
    lower: &'a Path,
    upper: &'a Path,
    work: &'a Path,
    merged: &'a Path,
    command: &'a str,
    mounts: &'a [BindMount],
    /// The environment the command runs under: the image's own env (PATH, VIRTUAL_ENV, …) merged with
    /// any caller overrides. Passed explicitly because a chrooted process inherits none of it otherwise.
    env: &'a [(String, String)],
}

/// How host-visible upperdir ownership translates into the execution root's view: under a single-uid
/// user namespace the command ran as mapped root, so files the host sees as `outer_uid`/`outer_gid`
/// were root-owned inside and must enter the layer as uid/gid 0.
#[derive(Debug, Clone, Copy)]
pub struct OwnerMap {
    outer_uid: u32,
    outer_gid: u32,
}

impl OwnerMap {
    fn map_uid(self, uid: u32) -> u32 {
        if uid == self.outer_uid { 0 } else { uid }
    }
    fn map_gid(self, gid: u32) -> u32 {
        if gid == self.outer_gid { 0 } else { gid }
    }
}

/// The swappable execution mechanism: run `plan.command` so its filesystem writes are captured in
/// `plan.upper`. Everything else in this module is mechanism-agnostic; only implementations of this
/// trait touch namespaces, mounts, and chroot.
pub trait Executor {
    fn execute(&self, plan: &ExecPlan) -> Result<()>;
    /// The ownership translation to apply when scanning the upperdir this executor produced;
    /// `None` when host ownership is already the in-root truth.
    fn owner_map(&self) -> Option<OwnerMap> {
        None
    }
}

/// Which execution mechanism to use. `Auto` prefers the direct privileged mount when a probe shows it
/// works (real ownership in the layer, no namespace edge cases) and falls back to the unprivileged
/// user-namespace executor otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorChoice {
    Auto,
    Userns,
    Privileged,
}

pub fn select_executor(choice: ExecutorChoice) -> Box<dyn Executor> {
    let (executor, label): (Box<dyn Executor>, &str) = match choice {
        ExecutorChoice::Userns => (Box::new(UsernsExecutor), "userns"),
        ExecutorChoice::Privileged => (Box::new(OverlayfsChroot), "privileged"),
        ExecutorChoice::Auto => {
            if privileged_overlay_available() {
                (Box::new(OverlayfsChroot), "privileged (auto: probe passed)")
            } else {
                (Box::new(UsernsExecutor), "userns (auto)")
            }
        }
    };
    tracing::info!(executor = label, "capture executor selected");
    executor
}

/// Whether the direct privileged path is really usable here: effective root AND a successful probe
/// overlay mount (root inside a locked-down container can still be denied mounts).
#[cfg(target_os = "linux")]
fn privileged_overlay_available() -> bool {
    // SAFETY: geteuid has no failure modes.
    if unsafe { libc::geteuid() } != 0 {
        return false;
    }
    let base = std::env::temp_dir().join(format!("forge-ovl-probe-{}", std::process::id()));
    let (lower, upper, work, merged) = (
        base.join("l"),
        base.join("u"),
        base.join("w"),
        base.join("m"),
    );
    let ready = [&lower, &upper, &work, &merged]
        .iter()
        .all(|d| std::fs::create_dir_all(d).is_ok());
    let data = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.display(),
        upper.display(),
        work.display()
    );
    let ok = ready && mount_overlay(&merged, &data).is_ok();
    if ok && let Ok(c) = path_cstring(&merged) {
        // SAFETY: `c` is a valid NUL-terminated path; umount2 only reads it.
        unsafe {
            libc::umount2(c.as_ptr(), libc::MNT_DETACH);
        }
    }
    let _ = std::fs::remove_dir_all(&base);
    ok
}

#[cfg(not(target_os = "linux"))]
fn privileged_overlay_available() -> bool {
    false
}

/// Configuration for a single diff capture.
pub struct CaptureConfig {
    pub base_ref: String,
    pub command: String,
    pub mounts: Vec<BindMount>,
    /// Caller env overrides layered on top of the image's own environment.
    pub env: Vec<(String, String)>,
    /// Destination repo (no tag); the pushed image is tagged by the diff's content and returned pinned.
    pub target_repo: String,
}

/// Capture the filesystem diff of running `cfg.command` against `cfg.base_ref`'s rootfs and push it as a
/// layer appended to the base, returning the digest-pinned ref. `work_root` holds the per-capture
/// overlay scratch (upper/work/merged). Registry IO runs on the cache's shared runtime handle. This is
/// the whole flow; the bin is a thin CLI over it.
pub fn capture_and_push(
    cfg: &CaptureConfig,
    cache: &RootfsCache,
    work_root: &Path,
    exec: &dyn Executor,
    base_auth: &RegistryAuth,
    target_auth: &RegistryAuth,
) -> Result<String> {
    sweep_stale_scratch(work_root);
    let rootfs = cache.ensure(&cfg.base_ref, base_auth)?;

    // Run in the image's own environment: the image's config Env is the base, caller overrides win.
    let mut env = cache
        .runtime()
        .block_on(crate::oci::image_env(&cfg.base_ref, base_auth))?;
    for (k, v) in &cfg.env {
        env.retain(|(ek, _)| ek != k);
        env.push((k.clone(), v.clone()));
    }

    let scratch = work_root.join(format!("capture-{}", std::process::id()));
    let upper = scratch.join("upper");
    let work = scratch.join("work");
    let merged = scratch.join("merged");
    for d in [&upper, &work, &merged] {
        std::fs::create_dir_all(d).with_context(|| format!("creating {d:?}"))?;
    }

    let created = prepare_bind_targets(rootfs.path(), &upper, &cfg.mounts)?;
    let plan = ExecPlan {
        lower: rootfs.path(),
        upper: &upper,
        work: &work,
        merged: &merged,
        command: &cfg.command,
        mounts: &cfg.mounts,
        env: &env,
    };
    tracing::info!(
        command = cfg.command,
        mounts = cfg.mounts.len(),
        "executing capture command"
    );
    let started = SystemTime::now();
    let run = exec.execute(&plan).context("executing the capture command");
    cleanup_bind_targets(&created);
    run?;
    tracing::info!(
        secs = started.elapsed().map(|d| d.as_secs()).unwrap_or(0),
        "capture command finished"
    );

    let entries =
        scan_upperdir(&upper, exec.owner_map()).context("scanning the captured upperdir")?;
    tracing::info!(entries = entries.len(), "captured filesystem diff");
    let tar = build_layer_tar(&entries)?;
    let layer = BuiltLayer::from_tar(&tar)?;
    // Tag by the layer's content so re-running the same capture is idempotent at the tag level; the
    // returned ref is digest-pinned regardless.
    let short = layer
        .diff_id()
        .trim_start_matches("sha256:")
        .get(..12)
        .unwrap_or("layer");
    let target_ref = format!("{}:cap-{short}", cfg.target_repo);

    let pushed = cache.runtime().block_on(crate::oci::push_layer(
        &cfg.base_ref,
        &target_ref,
        layer,
        "agentic diff-capture layer",
        base_auth,
        target_auth,
    ))?;

    let _ = std::fs::remove_dir_all(&scratch);
    Ok(pushed)
}

/// Remove crash-leftover `capture-*` scratch dirs older than [`STALE_SCRATCH_AGE`]. Best-effort.
fn sweep_stale_scratch(work_root: &Path) {
    let Ok(rd) = std::fs::read_dir(work_root) else {
        return;
    };
    let now = SystemTime::now();
    for e in rd.flatten() {
        if !e.file_name().to_string_lossy().starts_with("capture-") {
            continue;
        }
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .map(|age| age >= STALE_SCRATCH_AGE)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Pre-create bind-mount targets that exist in neither the lower rootfs nor the upper, creating them in
/// the upperdir (the merged view shows them, and both executors can then bind without writing through
/// the overlay). Returns the created paths deepest-first so [`cleanup_bind_targets`] can peel them back
/// off, keeping accidental empty dirs out of the captured diff.
fn prepare_bind_targets(lower: &Path, upper: &Path, mounts: &[BindMount]) -> Result<Vec<PathBuf>> {
    let mut created = Vec::new();
    for m in mounts {
        let rel = Path::new(m.container.trim_start_matches('/'));
        if rel.as_os_str().is_empty() {
            continue;
        }
        let comps: Vec<_> = rel.components().collect();
        let mut rel_so_far = PathBuf::new();
        for (i, comp) in comps.iter().enumerate() {
            rel_so_far.push(comp);
            if lower.join(&rel_so_far).exists() {
                continue;
            }
            let in_upper = upper.join(&rel_so_far);
            if in_upper.exists() {
                continue;
            }
            if i + 1 == comps.len() && m.host.is_file() {
                std::fs::File::create(&in_upper)
                    .with_context(|| format!("creating bind target file {in_upper:?}"))?;
            } else {
                std::fs::create_dir(&in_upper)
                    .with_context(|| format!("creating bind target dir {in_upper:?}"))?;
            }
            created.push(in_upper);
        }
    }
    created.reverse();
    Ok(created)
}

/// Remove pre-created bind targets that stayed empty (a dir the command actually wrote into is kept;
/// it's a real part of the diff). Best-effort by construction: `remove_dir` refuses non-empty dirs.
fn cleanup_bind_targets(created: &[PathBuf]) {
    for p in created {
        match std::fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => {
                let _ = std::fs::remove_dir(p);
            }
            Ok(_) => {
                let _ = std::fs::remove_file(p);
            }
            Err(_) => {}
        }
    }
}

/// Walk an overlayfs upperdir into the ordered [`LayerEntry`] diff it represents. Detects overlayfs
/// whiteout markers (a char device with rdev 0 -> a deletion) and opaque directories (the
/// `trusted.overlay.opaque` xattr). Parents are yielded before children (a top-down walk), the order
/// [`build_layer_tar`] needs. `owner` translates host-visible ownership into the execution root's view
/// (see [`OwnerMap`]).
fn scan_upperdir(upper: &Path, owner: Option<OwnerMap>) -> Result<Vec<LayerEntry>> {
    let mut entries = Vec::new();
    scan_dir(upper, upper, owner, &mut entries)?;
    Ok(entries)
}

fn scan_dir(
    root: &Path,
    dir: &Path,
    owner: Option<OwnerMap>,
    out: &mut Vec<LayerEntry>,
) -> Result<()> {
    let mut children: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading upperdir {dir:?}"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    // Deterministic order (read_dir is unordered), keeping parent-before-child across the recursion.
    children.sort();

    for path in children {
        let rel = path
            .strip_prefix(root)
            .with_context(|| format!("{path:?} escaped the upperdir root"))?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let meta = std::fs::symlink_metadata(&path).with_context(|| format!("stat {path:?}"))?;
        let ft = meta.file_type();
        let (uid, gid) = match owner {
            Some(m) => (m.map_uid(meta.uid()), m.map_gid(meta.gid())),
            None => (meta.uid(), meta.gid()),
        };

        if ft.is_char_device() && meta.rdev() == 0 {
            out.push(LayerEntry::Whiteout { path: rel_str });
            continue;
        }
        if ft.is_symlink() {
            let target = std::fs::read_link(&path).with_context(|| format!("readlink {path:?}"))?;
            out.push(LayerEntry::Symlink {
                path: rel_str,
                target: target.to_string_lossy().into_owned(),
                uid,
                gid,
            });
            continue;
        }
        if ft.is_dir() {
            let opaque = get_xattr(&path, "trusted.overlay.opaque").as_deref() == Some(b"y");
            out.push(LayerEntry::Dir {
                path: rel_str,
                mode: meta.mode() & 0o7777,
                uid,
                gid,
                opaque,
            });
            scan_dir(root, &path, owner, out)?;
            continue;
        }
        if ft.is_file() {
            let contents = std::fs::read(&path).with_context(|| format!("reading {path:?}"))?;
            out.push(LayerEntry::File {
                path: rel_str,
                mode: meta.mode() & 0o7777,
                uid,
                gid,
                contents,
            });
            continue;
        }
        // block/fifo/socket: not something a build writes into a layer; skip rather than guess.
    }
    Ok(())
}

/// Read an extended attribute via `lgetxattr` (not following symlinks); `None` if it's absent or the
/// filesystem doesn't support xattrs. std has no xattr API, hence the direct syscall. Only overlayfs
/// (Linux) sets `trusted.overlay.opaque`, so on other platforms there's nothing to read.
#[cfg(target_os = "linux")]
fn get_xattr(path: &Path, name: &str) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let cpath = CString::new(path.as_os_str().as_bytes()).ok()?;
    let cname = CString::new(name).ok()?;
    // SAFETY: cpath/cname are valid NUL-terminated strings; a null buffer with size 0 asks for the size.
    let size = unsafe { libc::lgetxattr(cpath.as_ptr(), cname.as_ptr(), std::ptr::null_mut(), 0) };
    if size <= 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    // SAFETY: buf has `size` bytes; lgetxattr writes at most that many.
    let got = unsafe {
        libc::lgetxattr(
            cpath.as_ptr(),
            cname.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
        )
    };
    if got <= 0 {
        return None;
    }
    buf.truncate(got as usize);
    Some(buf)
}

#[cfg(not(target_os = "linux"))]
fn get_xattr(_path: &Path, _name: &str) -> Option<Vec<u8>> {
    None
}

/// One bind into the merged root shared by both executors: the runtime pseudo-filesystems and resolver
/// config are best-effort (an image without the target path just skips them); the caller's source
/// mounts are required.
#[cfg(target_os = "linux")]
struct RuntimeBind {
    src: PathBuf,
    target: PathBuf,
    required: bool,
}

#[cfg(target_os = "linux")]
fn runtime_binds(plan: &ExecPlan) -> Vec<RuntimeBind> {
    let mut binds = Vec::new();
    for (src, rel) in [("/proc", "proc"), ("/dev", "dev"), ("/sys", "sys")] {
        if Path::new(src).exists() {
            binds.push(RuntimeBind {
                src: src.into(),
                target: plan.merged.join(rel),
                required: false,
            });
        }
    }
    if Path::new("/etc/resolv.conf").exists() {
        binds.push(RuntimeBind {
            src: "/etc/resolv.conf".into(),
            target: plan.merged.join("etc/resolv.conf"),
            required: false,
        });
    }
    for m in plan.mounts {
        binds.push(RuntimeBind {
            src: m.host.clone(),
            target: plan.merged.join(m.container.trim_start_matches('/')),
            required: true,
        });
    }
    binds
}

/// Direct kernel-overlayfs + chroot execution: mount the overlay and binds from THIS process, then run
/// the command chrooted into the merged view. Needs real mount capability (effective root outside any
/// confinement): the privileged fast path, chosen by [`select_executor`] only when a probe mount
/// succeeds.
pub struct OverlayfsChroot;

#[cfg(not(target_os = "linux"))]
impl Executor for OverlayfsChroot {
    fn execute(&self, _plan: &ExecPlan) -> Result<()> {
        bail!("overlayfs+chroot capture is only supported on Linux")
    }
}

#[cfg(target_os = "linux")]
impl Executor for OverlayfsChroot {
    fn execute(&self, plan: &ExecPlan) -> Result<()> {
        let mut guard = MountGuard::default();

        let data = format!(
            "lowerdir={},upperdir={},workdir={}",
            plan.lower.display(),
            plan.upper.display(),
            plan.work.display()
        );
        mount_overlay(plan.merged, &data)
            .context("mounting the overlay (privileged: needs mount capability)")?;
        guard.push(plan.merged);

        for b in runtime_binds(plan) {
            match bind_mount(&b.src, &b.target) {
                Ok(()) => guard.push(&b.target),
                Err(e) if b.required => {
                    return Err(e)
                        .with_context(|| format!("bind-mounting {:?} -> {:?}", b.src, b.target));
                }
                Err(_) => {}
            }
        }

        run_chrooted(plan.merged, plan.command, plan.env)?;
        Ok(())
    }
}

/// Unprivileged execution in a self-mapped user namespace: the child unshares
/// `CLONE_NEWUSER|CLONE_NEWNS`, writes its own single-uid root mapping (no newuidmap/newgidmap
/// binaries), mounts kernel overlayfs INSIDE the namespace (userns-mountable on kernels >= 5.11), binds,
/// and chroots. The default executor. The mount namespace dies with the command, so there is nothing to
/// unmount afterward.
pub struct UsernsExecutor;

#[cfg(not(target_os = "linux"))]
impl Executor for UsernsExecutor {
    fn execute(&self, _plan: &ExecPlan) -> Result<()> {
        bail!("user-namespace capture is only supported on Linux")
    }
}

#[cfg(target_os = "linux")]
impl Executor for UsernsExecutor {
    fn execute(&self, plan: &ExecPlan) -> Result<()> {
        use std::os::unix::process::CommandExt;

        let ops = NsOps::build(plan)?;
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(plan.command);
        cmd.env_clear();
        for (k, v) in plan.env {
            cmd.env(k, v);
        }
        // SAFETY: enter() uses only raw syscalls on CStrings/buffers built before the fork; no
        // allocation or locks run between fork and exec.
        unsafe {
            cmd.pre_exec(move || ops.enter());
        }
        let status = cmd.status().context(
            "user-namespace capture setup failed (the failing stage was printed above). Likely \
             causes: unprivileged user namespaces disabled (user.max_user_namespaces=0 / \
             kernel.unprivileged_userns_clone=0), seccomp blocking unshare, or AppArmor \
             confinement — on Kubernetes set the annotation \
             container.apparmor.security.beta.kubernetes.io/<container>: unconfined. Overlayfs \
             inside a user namespace also needs kernel >= 5.11",
        )?;
        if !status.success() {
            bail!("capture command exited with {status}");
        }
        Ok(())
    }

    fn owner_map(&self) -> Option<OwnerMap> {
        // SAFETY: geteuid/getegid have no failure modes.
        Some(OwnerMap {
            outer_uid: unsafe { libc::geteuid() },
            outer_gid: unsafe { libc::getegid() },
        })
    }
}

#[cfg(target_os = "linux")]
const SLASH: &CStr = c"/";
#[cfg(target_os = "linux")]
const OVERLAY: &CStr = c"overlay";

/// Everything the forked child needs to enter the namespace and set up its world, pre-built as
/// CStrings/byte buffers so `enter()` performs no allocation between fork and exec.
#[cfg(target_os = "linux")]
struct NsOps {
    proc_writes: Vec<(&'static CStr, Vec<u8>, &'static [u8])>,
    overlay_target: CString,
    overlay_data: CString,
    binds: Vec<NsBind>,
}

#[cfg(target_os = "linux")]
struct NsBind {
    src: CString,
    target: CString,
    required: bool,
    fail_msg: Vec<u8>,
}

#[cfg(target_os = "linux")]
impl NsOps {
    fn build(plan: &ExecPlan) -> Result<Self> {
        // SAFETY: geteuid/getegid have no failure modes.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        let proc_writes = vec![
            (
                c"/proc/self/setgroups",
                b"deny".to_vec(),
                b"writing /proc/self/setgroups failed\n".as_slice(),
            ),
            (
                c"/proc/self/gid_map",
                format!("0 {gid} 1\n").into_bytes(),
                b"writing /proc/self/gid_map failed\n".as_slice(),
            ),
            (
                c"/proc/self/uid_map",
                format!("0 {uid} 1\n").into_bytes(),
                b"writing /proc/self/uid_map failed\n".as_slice(),
            ),
        ];
        let overlay_data = CString::new(format!(
            "lowerdir={},upperdir={},workdir={}",
            plan.lower.display(),
            plan.upper.display(),
            plan.work.display()
        ))
        .context("overlay options contain an interior NUL")?;
        let mut binds = Vec::new();
        for b in runtime_binds(plan) {
            binds.push(NsBind {
                fail_msg: format!("bind mount failed: {}\n", b.target.display()).into_bytes(),
                src: path_cstring(&b.src)?,
                target: path_cstring(&b.target)?,
                required: b.required,
            });
        }
        Ok(Self {
            proc_writes,
            overlay_target: path_cstring(plan.merged)?,
            overlay_data,
            binds,
        })
    }

    /// Runs in the forked child, pre-exec: unshare into a self-mapped user+mount namespace, become
    /// mapped root, mount the overlay and binds, chroot. On failure, names the stage on stderr (raw
    /// `write`, inherited fd) and returns the syscall's errno.
    fn enter(&self) -> std::io::Result<()> {
        // SAFETY: every call below is a raw syscall on pre-built NUL-terminated strings/buffers.
        unsafe {
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                return Err(stage_err(b"unshare(CLONE_NEWUSER|CLONE_NEWNS) failed\n"));
            }
            for (path, data, msg) in &self.proc_writes {
                if let Err(e) = write_whole(path, data) {
                    libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
                    return Err(e);
                }
            }
            // Keep our mounts out of the parent namespace regardless of inherited propagation.
            libc::mount(
                std::ptr::null(),
                SLASH.as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            );
            if libc::mount(
                OVERLAY.as_ptr(),
                self.overlay_target.as_ptr(),
                OVERLAY.as_ptr(),
                0,
                self.overlay_data.as_ptr() as *const libc::c_void,
            ) != 0
            {
                return Err(stage_err(
                    b"overlay mount inside the user namespace failed (needs kernel >= 5.11 and \
                      upper/work on a non-overlay filesystem)\n",
                ));
            }
            for b in &self.binds {
                if libc::mount(
                    b.src.as_ptr(),
                    b.target.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND | libc::MS_REC,
                    std::ptr::null(),
                ) != 0
                    && b.required
                {
                    return Err(stage_err(&b.fail_msg));
                }
            }
            if libc::chroot(self.overlay_target.as_ptr()) != 0 {
                return Err(stage_err(b"chroot into the merged root failed\n"));
            }
            if libc::chdir(SLASH.as_ptr()) != 0 {
                return Err(stage_err(b"chdir / failed\n"));
            }
        }
        Ok(())
    }
}

/// Capture errno FIRST (the diagnostic write would clobber it), then name the failed stage on stderr.
#[cfg(target_os = "linux")]
unsafe fn stage_err(msg: &[u8]) -> std::io::Error {
    let e = std::io::Error::last_os_error();
    // SAFETY: fd 2 is inherited and open; write only reads the buffer.
    unsafe {
        libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
    }
    e
}

/// open+write+close via raw syscalls (fork-safe): the uid_map/gid_map/setgroups writes.
#[cfg(target_os = "linux")]
unsafe fn write_whole(path: &CStr, data: &[u8]) -> std::io::Result<()> {
    // SAFETY: path is NUL-terminated; write only reads data; close takes the fd we opened.
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let n = libc::write(fd, data.as_ptr() as *const libc::c_void, data.len());
        let err = std::io::Error::last_os_error();
        libc::close(fd);
        if n == data.len() as isize {
            Ok(())
        } else {
            Err(err)
        }
    }
}

/// Run `command` via `/bin/sh -c` chrooted into `merged`, with a cleared-then-set environment. Streams
/// the command's stdout/stderr through so a build failure is visible. A non-zero exit is an error (the
/// captured diff would be partial/meaningless).
#[cfg(target_os = "linux")]
fn run_chrooted(merged: &Path, command: &str, env: &[(String, String)]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let merged_c = path_cstring(merged)?;

    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(command);
    cmd.env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    // SAFETY: chroot/chdir are async-signal-safe and touch only pre-built CStrings; no allocation or
    // locks run between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::chroot(merged_c.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::chdir(SLASH.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let status = cmd
        .status()
        .context("spawning `/bin/sh` inside the chroot")?;
    if !status.success() {
        bail!("capture command exited with {status}");
    }
    Ok(())
}

/// Unmounts every mount it was handed, in reverse order, on drop, so a bailed-out capture never leaves
/// an overlay or bind mounted. Lazy (`MNT_DETACH`) so a still-busy mount detaches instead of failing.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct MountGuard {
    targets: Vec<CString>,
}

#[cfg(target_os = "linux")]
impl MountGuard {
    fn push(&mut self, target: &Path) {
        if let Ok(c) = path_cstring(target) {
            self.targets.push(c);
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for MountGuard {
    fn drop(&mut self) {
        for target in self.targets.iter().rev() {
            // SAFETY: `target` is a valid NUL-terminated path; umount2 only reads it.
            unsafe {
                libc::umount2(target.as_ptr(), libc::MNT_DETACH);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn path_cstring(path: &Path) -> Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path {path:?} contains an interior NUL"))
}

#[cfg(target_os = "linux")]
fn mount_overlay(target: &Path, data: &str) -> Result<()> {
    let target_c = path_cstring(target)?;
    let data_c = CString::new(data).context("overlay options contain an interior NUL")?;
    // SAFETY: all pointers are valid NUL-terminated strings kept alive across the call.
    let rc = unsafe {
        libc::mount(
            OVERLAY.as_ptr(),
            target_c.as_ptr(),
            OVERLAY.as_ptr(),
            0,
            data_c.as_ptr() as *const libc::c_void,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("overlay mount failed");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn bind_mount(src: &Path, target: &Path) -> Result<()> {
    let src_c = path_cstring(src)?;
    let target_c = path_cstring(target)?;
    // SAFETY: src/target are valid NUL-terminated strings; a recursive bind reads them and no data ptr.
    let rc = unsafe {
        libc::mount(
            src_c.as_ptr(),
            target_c.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("bind mount failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn rootfs_paths_key_by_digest_with_a_sibling_marker() {
        let (dir, marker) = rootfs_paths(Path::new("/cache"), "sha256:abc123");
        assert_eq!(dir, Path::new("/cache/rootfs/sha256_abc123"));
        assert_eq!(marker, Path::new("/cache/rootfs/sha256_abc123.done"));
        // The marker is a sibling of the rootfs dir, never inside it.
        assert_eq!(marker.parent(), dir.parent());
    }

    #[test]
    fn eviction_keeps_newest_and_respects_the_idle_guard() {
        let now = SystemTime::now();
        let ago = |secs: u64| now - Duration::from_secs(secs);
        let entries = vec![
            ("oldest".to_string(), ago(10 * 24 * 3600)),
            ("fresh".to_string(), ago(3600)),
            ("mid".to_string(), ago(7 * 3600)),
            ("old".to_string(), ago(2 * 24 * 3600)),
        ];
        // keep=2 protects the two most recent (fresh, mid); the rest are idle > 6h -> evicted, oldest included.
        let mut v = eviction_victims(&entries, 2, now, Duration::from_secs(6 * 3600));
        v.sort();
        assert_eq!(v, vec!["old".to_string(), "oldest".to_string()]);

        // The idle guard: an entry used 1h ago is never evicted even when over budget.
        let v = eviction_victims(&entries, 0, now, Duration::from_secs(6 * 3600));
        assert!(!v.contains(&"fresh".to_string()));
        assert!(v.contains(&"mid".to_string()));

        // Under budget: nothing to evict.
        assert!(eviction_victims(&entries, 4, now, Duration::ZERO).is_empty());
    }

    #[test]
    fn prepare_creates_missing_targets_in_upper_and_cleanup_peels_empties() {
        let base = std::env::temp_dir().join(format!("forge-prep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let lower = base.join("lower");
        let upper = base.join("upper");
        std::fs::create_dir_all(lower.join("existing")).unwrap();
        std::fs::create_dir_all(&upper).unwrap();

        let mounts = vec![
            BindMount {
                host: base.clone(),
                container: "/existing".into(),
            },
            BindMount {
                host: base.clone(),
                container: "/workspace/src".into(),
            },
        ];
        let created = prepare_bind_targets(&lower, &upper, &mounts).unwrap();

        // /existing lives in lower: nothing created. /workspace/src: both components created in upper.
        assert_eq!(created.len(), 2);
        assert!(upper.join("workspace/src").is_dir());
        assert!(!upper.join("existing").exists());
        // Deepest-first, so cleanup can remove children before parents.
        assert_eq!(created[0], upper.join("workspace/src"));
        assert_eq!(created[1], upper.join("workspace"));

        cleanup_bind_targets(&created);
        assert!(!upper.join("workspace").exists(), "empty targets peeled");

        // A dir the command wrote into is part of the diff and survives cleanup.
        let created = prepare_bind_targets(&lower, &upper, &mounts[1..]).unwrap();
        std::fs::write(upper.join("workspace/src/out.txt"), b"kept").unwrap();
        cleanup_bind_targets(&created);
        assert!(upper.join("workspace/src/out.txt").exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn scan_upperdir_maps_each_kind_to_an_entry() {
        let base = std::env::temp_dir().join(format!("forge-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let upper = base.join("upper");
        std::fs::create_dir_all(upper.join("nested")).unwrap();
        std::fs::write(upper.join("plain.txt"), b"hello").unwrap();
        let exe = upper.join("nested/run.sh");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("plain.txt", upper.join("link")).unwrap();

        let entries = scan_upperdir(&upper, None).unwrap();

        // Nested dir yielded before its child (parent-before-child ordering).
        let nested_idx = entries
            .iter()
            .position(|e| matches!(e, LayerEntry::Dir { path, .. } if path == "nested"))
            .expect("nested dir present");
        let child_idx = entries
            .iter()
            .position(|e| matches!(e, LayerEntry::File { path, .. } if path == "nested/run.sh"))
            .expect("nested file present");
        assert!(nested_idx < child_idx, "parent must precede child");

        assert!(entries.iter().any(|e| matches!(e,
            LayerEntry::File { path, mode, contents, .. }
            if path == "plain.txt" && *mode == 0o644 && contents == b"hello")));
        // The executable bit is read off the upperdir file.
        assert!(entries.iter().any(|e| matches!(e,
            LayerEntry::File { path, mode, .. } if path == "nested/run.sh" && *mode == 0o755)));
        assert!(entries.iter().any(|e| matches!(e,
            LayerEntry::Symlink { path, target, .. } if path == "link" && target == "plain.txt")));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn scan_upperdir_applies_the_owner_map() {
        let base = std::env::temp_dir().join(format!("forge-scan-own-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let upper = base.join("upper");
        std::fs::create_dir_all(&upper).unwrap();
        std::fs::write(upper.join("f"), b"x").unwrap();

        // Files this test writes are owned by us, the "outer" identity a userns capture runs as.
        let meta = std::fs::metadata(upper.join("f")).unwrap();
        let owner = OwnerMap {
            outer_uid: meta.uid(),
            outer_gid: meta.gid(),
        };
        let entries = scan_upperdir(&upper, Some(owner)).unwrap();
        assert!(entries.iter().any(|e| matches!(e,
            LayerEntry::File { path, uid, gid, .. } if path == "f" && *uid == 0 && *gid == 0)));

        // A uid the map doesn't cover passes through untouched.
        assert_eq!(owner.map_uid(meta.uid().wrapping_add(1)), meta.uid() + 1);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn scan_upperdir_round_trips_through_a_built_layer() {
        // Scanning an upperdir then applying the built layer over an empty lower reproduces the tree.
        let base = std::env::temp_dir().join(format!("forge-scan-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let upper = base.join("upper");
        std::fs::create_dir_all(upper.join("opt/app")).unwrap();
        std::fs::write(upper.join("opt/app/config"), b"k=v").unwrap();

        let entries = scan_upperdir(&upper, None).unwrap();
        let tar = build_layer_tar(&entries).unwrap();

        let lower = base.join("lower");
        std::fs::create_dir_all(&lower).unwrap();
        crate::layer::apply_layer_reader(tar.as_slice(), &lower).unwrap();
        assert_eq!(std::fs::read(lower.join("opt/app/config")).unwrap(), b"k=v");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Can this environment unshare a self-mapped user namespace AND mount overlayfs inside it, with
    /// upper/work on the filesystem under `probe_base`? Probes with a trivial child (no chroot) so the
    /// smoke test can skip (not fail) under seccomp/AppArmor/userns-disabled kernels, and under an
    /// overlayfs `/tmp` (a container), where overlay refuses an overlay-backed upperdir.
    #[cfg(target_os = "linux")]
    fn can_userns_overlay(probe_base: &Path) -> bool {
        use std::os::unix::process::CommandExt;
        let dirs = ["l", "u", "w", "m"].map(|d| probe_base.join(d));
        if !dirs.iter().all(|d| std::fs::create_dir_all(d).is_ok()) {
            return false;
        }
        let Ok(data) = CString::new(format!(
            "lowerdir={},upperdir={},workdir={}",
            dirs[0].display(),
            dirs[1].display(),
            dirs[2].display()
        )) else {
            return false;
        };
        let Ok(target) = path_cstring(&dirs[3]) else {
            return false;
        };
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        let uid_map = format!("0 {uid} 1\n").into_bytes();
        let gid_map = format!("0 {gid} 1\n").into_bytes();
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg("true");
        unsafe {
            cmd.pre_exec(move || {
                if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                write_whole(c"/proc/self/setgroups", b"deny")?;
                write_whole(c"/proc/self/gid_map", &gid_map)?;
                write_whole(c"/proc/self/uid_map", &uid_map)?;
                if libc::mount(
                    OVERLAY.as_ptr(),
                    target.as_ptr(),
                    OVERLAY.as_ptr(),
                    0,
                    data.as_ptr() as *const libc::c_void,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.status().map(|s| s.success()).unwrap_or(false)
    }

    /// End-to-end smoke of the unprivileged executor: overlay a temp lower, bind the host toolchain
    /// dirs so the chroot has a shell, run a write, and assert the diff landed in upper as ns-root.
    #[cfg(target_os = "linux")]
    #[test]
    fn userns_executor_captures_a_write_unprivileged() {
        let base = std::env::temp_dir().join(format!("forge-userns-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        if !can_userns_overlay(&base.join("probe")) {
            println!(
                "skipping: this environment cannot unshare a user namespace or mount overlayfs \
                 inside one (seccomp/AppArmor/sysctl, or upper/work on an overlayfs like a \
                 container's /tmp)"
            );
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        let _ = std::fs::remove_dir_all(&base);
        let lower = base.join("lower");
        let upper = base.join("upper");
        let work = base.join("work");
        let merged = base.join("merged");
        for d in [&lower, &upper, &work, &merged] {
            std::fs::create_dir_all(d).unwrap();
        }

        let mounts: Vec<BindMount> = ["/usr", "/bin", "/lib", "/lib64", "/etc"]
            .iter()
            .filter(|p| Path::new(p).exists())
            .map(|p| BindMount {
                host: PathBuf::from(p),
                container: (*p).to_string(),
            })
            .collect();
        let created = prepare_bind_targets(&lower, &upper, &mounts).unwrap();
        let env = vec![("PATH".to_string(), "/usr/bin:/bin".to_string())];
        let plan = ExecPlan {
            lower: &lower,
            upper: &upper,
            work: &work,
            merged: &merged,
            command: "echo captured > /out.txt",
            mounts: &mounts,
            env: &env,
        };
        UsernsExecutor
            .execute(&plan)
            .expect("userns capture should succeed where unshare works");
        cleanup_bind_targets(&created);

        let entries = scan_upperdir(&upper, UsernsExecutor.owner_map()).unwrap();
        assert!(
            entries.iter().any(|e| matches!(e,
                LayerEntry::File { path, uid, gid, contents, .. }
                if path == "out.txt" && *uid == 0 && *gid == 0 && contents == b"captured\n")),
            "expected out.txt owned by mapped root in the diff, got {entries:?}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
