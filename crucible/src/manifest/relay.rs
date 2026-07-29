use serde::Deserialize;

/// One declared relay file: rendered into the workspace, 0600, git-excluded. Exactly one source:
/// `template` (with `${env:}`/`${file:}`/`${file64:}`/`${cmd:}` interpolation), `from_file`
/// (verbatim copy), or `from_cmd` (the file *is* a host command's stdout).
#[derive(Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct RelayFile {
    /// Destination path inside the sandbox (e.g. `.kube/config` → `~/.kube/config`). The
    /// rendered file is targeted-uploaded there per turn; it never enters the workspace/repo.
    pub dest: String,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub from_file: Option<String>,
    #[serde(default)]
    pub from_cmd: Option<String>,
}
