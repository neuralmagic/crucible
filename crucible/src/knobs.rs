//! The agent settings a manifest, a launch, and a task each name.

/// Which agent harness runs the turn. `claude` is the default everywhere; `codex` (OpenAI's
/// Codex CLI) is selected per-domain for harness ablations; `opencode` and `pi` are the harnesses
/// that speak either API family through a provider table.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    clap::ValueEnum,
    serde::Deserialize,
    serde::Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    #[default]
    Claude,
    Codex,
    #[value(name = "opencode")]
    OpenCode,
    Pi,
}

impl Harness {
    /// The manifest and CLI spelling of the harness.
    pub const fn as_str(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::OpenCode => "opencode",
            Harness::Pi => "pi",
        }
    }
}

/// The agent's reasoning-effort tier, passed to Claude Code as `--effort <level>`. A closed set
/// (Claude Code 2.1: low|medium|high|xhigh|max); unset means we don't pass the flag and Claude
/// Code picks its own default. Per-domain because the right tier is task-dependent (mechanical
/// remove-deprecated fixes need ~none; algorithmic issues want real reasoning), and it's a harness
/// hyperparameter worth ablating: does more thinking find better fixes, or just better reward hacks?
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    /// The exact token Claude Code's `--effort` flag expects.
    pub fn as_flag(self) -> &'static str {
        match self {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Xhigh => "xhigh",
            ReasoningEffort::Max => "max",
        }
    }
}
