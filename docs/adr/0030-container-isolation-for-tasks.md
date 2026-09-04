# ADR 0030: Container isolation for deterministic tasks

**Status:** Proposed (2026-09-04). Governance source: `gov/adr/ADR-0026`.
**Date:** 2026-09-04
**Related:** [ADR-0029](./0029-approval-gates.md) (the pod-dispatch path this waits on),
[ADR-0010](./0010-candidate-portfolios-and-search.md) (the second-runner mistake to avoid),
[work graphs](../work-graphs.md)

## Context

A `command` task runs `sh -c` in the run's workspace: in the loop pod, as the loop's user, on
the loop's network, with whatever toolchain the loop image baked. A pack needing a different
toolchain can either get it into the shared loop image or vendor it into the pack. The first
makes the loop image the union of every domain's dependencies; the second makes every pack carry
a build.

The plan contract already fixes what a task hands back: the fields it declares in `emits`, and
the workspace files it declares in `emits_files`. Nothing else crosses. That is exactly what
makes *where* a task ran a substrate detail rather than a contract one.

The engine already knows how to run work in a container, twice over. `isolated = true` means
"run somewhere disposable, keep only the declared output" — but `Isolation` is a one-variant
enum whose only member is a git worktree on the same filesystem, image, user and network as the
loop. Separately, an agent turn runs in a real container: `ComputeDriver` picks podman nested in
the loop pod or a sibling pod in-cluster, and the sandbox module already moves a workdir in and
named files out.

The boundary exists, the transfer exists, and the output contract that makes substitution safe
exists. What is missing is a deterministic task's ability to ask for it.

## Proposal

Give `command` and `evaluate` an optional `image`. Supplied, the task runs in a container from
that image, receiving the staged workspace and returning its declared JSON and files. Omitted,
nothing changes — the task runs on the loop pod as today, and every existing pack renders
byte-identically.

Model it as a second variant of the isolation enum, not an independent flag, so "where does this
run" stays one question with one dispatch point. Placement reuses `ComputeDriver`, so a pack
never names a substrate. The image pins to a digest at render time like every other image the
renderer emits.

## Consequences

A pack brings its own toolchain, so the loop image stops accreting one. A slow, flaky, or
hostile command cannot corrupt the shared workspace or reach the loop pod's network. The
declared output stops being a convention and becomes the enforced boundary it was always
described as.

Against that: a container start per task, image pull as a new failure mode, and a workspace
round-trip that makes a file-heavy task slower than a worktree. `emits_files` becomes
load-bearing — a task that forgets to declare a file silently loses it, where today it would
have left it in the shared workspace and a dependent would have found it anyway.

The risk worth naming is two dispatchers for one task kind. Landing this as a parallel path
beside the shell runner, rather than a second variant behind one dispatch point, repeats what
the wide tournament cost: a second runner over the same type, and a second thing every later
change has to be taught.

Open: whether an imaged task may share the workspace instead of being isolated; which secrets
and environment reach it; and whether its changes join the run's git memory, which the playbook
lane commits only for a task that settles passing.

This wants the pod-dispatch path the approval-gate work is about to change in the controller. It
should be specified after that stack merges, not stacked on it.
