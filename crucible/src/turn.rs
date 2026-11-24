//! Running one agent turn on the backend the run names: the sandboxed OpenShell flow, or a child
//! spawned here.

use crate::agent::AgentSource;
use crate::agent::event::{AgentEvent, RawStream};
use crate::agent::turn::TurnOutcome;
use crate::args::{Args, Paths};
use crucible::deadline::Deadline;

#[cfg(feature = "autoresearch")]
/// Run one agent turn against the source resolved from `args`. `sink(raw_line, stream,
/// event)` is called per output line; returns the turn's [`TurnOutcome`]: the highest cost the
/// agent reported (0 if none) plus the transport failure that stopped it, if any.
pub fn run_turn(
    args: &Args,
    p: &Paths,
    prompt: &str,
    json: bool,
    sink: impl FnMut(&str, RawStream, Option<&AgentEvent>),
) -> TurnOutcome {
    run_turn_with_session(args, p, prompt, json, None, None, sink)
}

/// Run a turn using a prepared session; the caller commits successful transport afterward. A turn
/// still running at `deadline` is killed and fails with [`crate::agent::turn::TurnFailure::DeadlineExceeded`].
pub(crate) fn run_turn_with_session(
    args: &Args,
    p: &Paths,
    prompt: &str,
    json: bool,
    session: Option<&crate::agent::agent_session::SessionTurn>,
    deadline: Option<Deadline>,
    sink: impl FnMut(&str, RawStream, Option<&AgentEvent>),
) -> TurnOutcome {
    let source = args.agent_source();
    // The openshell driver owns a multi-step turn (gateway/provider/sandbox/exec/download),
    // not a single streamed child, delegate to its module rather than the generic path. It
    // reaches the engine runtime handle itself (see `crate::agent::engine::handle`).
    if source == AgentSource::OpenshellDriver {
        return crate::openshell_turn::run::turn(
            &args.sandbox_turn(),
            p,
            prompt,
            json,
            session,
            deadline,
            sink,
        );
    }
    crate::agent::run_turn_with(&source, args, p, prompt, json, session, deadline, sink)
}
