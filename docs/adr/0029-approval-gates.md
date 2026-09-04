# ADR 0029: Plan-authored approval gates on an event-sourced loop

**Status:** Accepted; implemented (2026-09-04). Governance source: `gov/adr/ADR-0025`.
**Date:** 2026-09-04
**Related:** [ADR-0018](./0018-session-log-as-the-record.md) (the log this makes authoritative),
[ADR-0004](./0004-core-loop-state-model.md) (amended: dispatch may outlive its process),
[ADR-0003](./0003-frozen-judge.md), [RFC-0002 C-PLAYBOOK-APPROVAL](../rfc/RFC-0002.md)

## Context

A playbook could already ask a human for something. The provisioning ask parks the loop on a
marker and waits for a re-scope over the control bridge. That path belongs to the scored loop,
it is one global wait rather than a step in the graph, and the only thing that can end it is an
operator at a socket.

The work that needs gating is not shaped like that. A pack wants to say "run these tasks, then
wait for someone to approve the change, then deploy", and the approval usually already exists
somewhere else: a pull request got a review, a tracker issue moved to Ready. It is a node in the
graph, with dependents, a verdict contribution, and a name.

Waiting is expensive in the shape the controller runs playbooks. A pod idling for a day on a
human holds a scheduled slot and a node. A run that can leave and come back is worth more than
one that can only idle, but leaving requires state that outlives the process.

By this point three folds of the session log had grown up independently: the resume fold rebuilt
counters, a tail scan classified crashes, and the controller's ingest had its own event enum. A
gate adds events all three would have to learn. ADR-0018 made the session log the run's record;
nothing had yet made it the run's state.

## Decision

Add an approval gate as a task kind the author declares, and make the session log the single
fold every consumer reads.

The fold moves into the contract: one exhaustive `apply` over the event vocabulary, one
classifier, one resume view. The engine and the controller read the same code, so a new event
kind is taught to one place. The scored loop's decisions move out of the driver into a state
machine with no I/O, which the driver hosts by performing effects and handing back results.

A gate names what may resolve it. An operator acting on the run always can; a gate may
additionally name a pull request or a tracker issue, and whichever arrives first wins.
Resolution is keyed by a trace id derived from the run and the task, and is idempotent under
that key, so a retrying resolver, a second source, and a resumed process replaying a decision
all converge on one recorded outcome. Granted settles the gate passing; denied and timeout
settle it failing, and the ordinary verdict rule does the rest. No fourth outcome, no new task
status.

A run at a gate either waits in place or suspends. Suspending writes the workspace and the state
dir to the controller's existing artifact drop-box and exits zero under a distinct shutdown
outcome; a later process restores them and is handed the decisions the controller settled
meanwhile. Waiting time does not count against the wall-clock ceiling.

## Consequences

A pack can express "a person decides here" as a node rather than as a task that polls something.
The four folds become one, so the controller and the engine cannot disagree about what a log
says. Moving the loop's decisions somewhere testable turned up an attempt bound that had been
counting per process rather than per iteration.

The feature is half usable until the controller carries it. The engine can suspend, but nothing
resumes it without the controller's approval rows, its suspended status, and its artifact
endpoints, so the core pin bump and that work are one delivery rather than two.

A gate makes a run's wall clock unbounded in practice: the ceiling stops counting while a gate
is open, and the only backstop is the gate's own timeout.

The wire stays at one version. Every addition is an optional field or a new token, and readers
must skip event kinds they do not know, which is what makes an older consumer safe against a
newer writer.
