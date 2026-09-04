//! The per-workspace sandbox name plus the argv for the file-transfer subcommands that stay on the
//! `openshell` CLI. Sandbox lifecycle, exec, policy and logs moved to the gateway's gRPC API
//! ([`super::grpc`]); only upload/download remain here, because file transfer is SSH-tar inside
//! the CLI with no equivalent RPC. The spawning lives in [`super::run`]; this module just builds
//! the argv after the `openshell` program name.
//!
//! The per-turn targeted upload (`upload <name> <local> <dest>`) places a rendered cluster cred
//! straight at its sandbox path, never through the workspace/repo.

/// One random identity for this engine process. Kubernetes containers commonly reuse the same
/// small PID, so a PID cannot distinguish a relaunched run from the sandbox it left behind.
fn run_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| uuid::Uuid::now_v7().simple().to_string())
}

/// The gateway's cap on a sandbox name: three routable names and two `--` delimiters have to fit
/// one 63-character DNS label.
pub const MAX_NAME_LEN: usize = 19;
const _: () = assert!("ci-".len() + 16 == MAX_NAME_LEN);

/// The per-workspace sandbox name: `ci-` plus a 64-bit hash of the run id and the workspace,
/// exactly [`MAX_NAME_LEN`] long. Stable across turns within one loop instance (the process
/// identity and workspace are fixed), unique across parallel candidates and relaunched processes
/// sharing a gateway. The broker gets this exact string at spawn (`BROKER_SANDBOX_NAME`) rather
/// than re-deriving it; neither component is a durable ID.
pub fn name_for(workspace: &std::path::Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    run_id().hash(&mut h);
    workspace.hash(&mut h);
    format!("ci-{:016x}", h.finish())
}

/// `sandbox upload --no-git-ignore <name> <local>`: push the whole workspace to `~`, dotfiles
/// included (the `--no-git-ignore` Regular/tar path), so the agent sees the full tree.
pub fn workdir_upload_args(name: &str, local: &str) -> Vec<String> {
    vec![
        "sandbox".into(),
        "upload".into(),
        "--no-git-ignore".into(),
        name.into(),
        local.into(),
    ]
}

/// `sandbox upload <name> <local> <dest>`: targeted single-file upload (the rendered cred) to
/// an explicit sandbox path, e.g. `.kube/config` → `~/.kube/config`. No `--no-git-ignore`: the
/// source is a host temp file outside any repo, so the default path uploads it verbatim and
/// `dest`'s parent dir is created on extract.
pub fn file_upload_args(name: &str, local: &str, dest: &str) -> Vec<String> {
    vec![
        "sandbox".into(),
        "upload".into(),
        name.into(),
        local.into(),
        dest.into(),
    ]
}

/// `sandbox download <name> <sandbox_path> <local>`: copy the sandbox workdir back to the host
/// after the turn (so kept iterations are committed from the host workspace).
pub fn download_args(name: &str, sandbox_path: &str, local: &str) -> Vec<String> {
    vec![
        "sandbox".into(),
        "download".into(),
        name.into(),
        sandbox_path.into(),
        local.into(),
    ]
}

#[cfg(test)]
mod tests {
    use crate::openshell::sandbox::*;

    #[test]
    fn name_for_is_per_workspace_and_run() {
        use std::path::Path;
        let a = name_for(Path::new("/state/worktrees/lane-a"));
        let b = name_for(Path::new("/state/worktrees/lane-b"));
        assert_ne!(a, b, "wide-round lanes get distinct sandboxes");
        assert_eq!(
            a,
            name_for(Path::new("/state/worktrees/lane-a")),
            "stable across turns within a lane"
        );
        assert!(a.starts_with("ci-"));
        assert_eq!(
            a.len(),
            MAX_NAME_LEN,
            "fills the gateway's routable cap: {a}"
        );
        assert!(
            a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "container-name safe: {a}"
        );
    }

    #[test]
    fn workdir_upload_keeps_gitignored_dotfiles() {
        let v = workdir_upload_args("ci", "/ws");
        assert_eq!(v, ["sandbox", "upload", "--no-git-ignore", "ci", "/ws"]);
    }

    #[test]
    fn file_upload_is_targeted_without_git_ignore_flag() {
        let v = file_upload_args("ci", "/tmp/kubeconfig.123", ".kube/config");
        assert_eq!(
            v,
            [
                "sandbox",
                "upload",
                "ci",
                "/tmp/kubeconfig.123",
                ".kube/config"
            ]
        );
        assert!(
            !v.contains(&"--no-git-ignore".to_string()),
            "single host file, no repo"
        );
    }

    #[test]
    fn download_round_trips_the_workdir() {
        let v = download_args("ci", "/sandbox/ws", "/ws");
        assert_eq!(v, ["sandbox", "download", "ci", "/sandbox/ws", "/ws"]);
    }
}
