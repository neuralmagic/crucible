//! crucible, a propose→apply→measure→accept/reject→remember loop. An LLM agent proposes a
//! change to a reversibly-mutable world; a frozen judge measures it; the engine keeps or
//! discards by a generic rule and remembers the winners. A domain is a `crucible.toml`
//! manifest (see `docs/crucible-contract.md`), not engine code; `examples/counter` is the
//! litmus manifest.
//!
//! Thin driver, fat agent: each iteration hands the agent (Claude) a goal +
//! history + a toolbox, then independently runs the manifest's `measure` command and gates
//! keep/discard. The engine names nothing domain-specific; everything plugs in behind the
//! [`crucible::World`] + [`crucible::Judge`] traits, satisfied by the built-in command
//! batteries the manifest configures.
//!
//! Layout: this file is the entrypoint. [`args`] is the shared vocabulary ([`args::Args`],
//! [`args::Paths`], [`args::Prepared`]); [`cli`] parses the command line and dispatches it;
//! [`runloop`] holds the single orchestration loop; [`agent`] runs one turn; [`control`] steers a
//! running loop from outside; [`report`] is how the loop talks to a human or a log; [`scope`]
//! is the scoping pipeline. The loop talks only to a [`report::Reporter`], so one loop drives
//! multiple front-ends: [`report::console::ConsoleReporter`] for headless runs and the NDJSON
//! [`report::stream::SessionReporter`] for stdout/session-log runs. The choice is just `--ui`
//! (default: auto by TTY).
//!
//! Operator ergonomics:
//!
//! - Ctrl+C never just dies: it stops cleanly after the current step and prints a summary. Headless offers a steer/quit prompt.
//! - Steering: drop guidance in STEER.md (or via the prompt) and it is injected into the next iteration's prompt, the lever for when the agent goes off the rails.

mod agent;
mod args;
mod cli;
mod control;
mod identity;
mod process;
mod report;
mod runloop;
mod scope;
#[cfg(test)]
mod testing;
pub(crate) use crucible_harness::stream_json;

use crucible::{deploy, duration, errors, exposure, flow, manifest, outputs, turn_trace};

/// The OpenShell turn runtime. The gateway client and the egress policy are library code (the
/// deploy renderer needs their constants); the per-turn flow, provider, and sandbox helpers run
/// only here.
mod openshell {
    pub use crucible::openshell::{gateway, grpc, policy};

    pub mod provider;
    pub mod run;
    pub mod sandbox;
}

/// The plan runtime: the CLI and the agent-harness task runner over the library's plan IR,
/// compiler, and executor.
mod plan {
    pub use crucible::plan::{
        STAGED_INPUTS, TASK_NAME_ENV, exec, ir, runner, starlark, term_img, workflow, worktree,
    };

    pub mod cli;
    pub mod events;
    pub mod harness;
}

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    cli::run::dispatch(cli::Cli::parse())
}
