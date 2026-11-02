# Bounded outputs: current state

This page describes what `[outputs]` and the capability disclosure do, what they do not do,
and how they compare to the safe outputs of GitHub Agentic Workflows. The normative
specification is RFC-0001 `C-OUTPUTS` and `C-CAPABILITY-DISCLOSURE`. This page summarizes the
current behavior and the integration contract.

## The model

In the usual case, an agent does not change the outside world directly. Each side effect
leaves a run as a broker tool call that the agent requests, or as an action that the engine
does (publishing a kept candidate, dispatching a build). Some packs need exceptions: an
agent-visible credential, added egress, a relayed kubeconfig. Every exception must appear in
the disclosure.

In `[outputs]`, the pack declares, before the run starts, how many writes of each kind its
runs can make and their targets. The broker and the engine both check this ledger before they
act. A write that goes over its count or to a different target fails with the reason, and
the run continues. Some reach cannot be counted, for example a raw credential or network
access. The pack must declare it instead. That declaration is the exposure. The person who
approves the run reads it, and a launch fails if it gives a grant that the exposure does not
list.

```
        frozen [outputs] ────────────┬──────────────┐
                                     v              v
agent ── asks tool ──> broker ──> tally ──ok──> jira/slack/registry/rig
                                     │
engine ─ publish/dispatch ──────> tally ──ok──> draft PR / workflow run
                                     │
                            over / off-target
                                     v
                     refuse, name the bound, log it, run continues

uncountable reach (credentials, egress, relays)
        └──> declared as exposure ──> `crucible plan exposure` emits it as JSON
                  └──> an orchestrator stores, diffs, and shows it at approval
```

## What it does

**Declaration.** In `[outputs]`, a pack can declare a count for each run and a target for
each output kind. The vocabulary is closed, and the engine version defines it: `draft-pr`,
`tracker-comment`, `chat-message`, `image-push`, `deploy`, `workflow-dispatch`,
`gpu-capture`. These are manifest errors: a kind outside the vocabulary, a declaration
without a count, an unlimited target (`*` or `**`, fixed or open), and a scope for any
target. An open target names a scope and can bind to a workflow parameter. In that case, the
target of the write must be equal to the run value of that parameter. Undeclared kinds get
their values from a documented default table in the engine. Every default has a count, and
no default has an open target.

**Enforcement.** The bounds load from the frozen manifest. Two mediation points that the
agent cannot reach enforce them:

- the broker, for the kinds that tools write (`tracker-comment`, `chat-message`,
  `image-push`, `deploy`, `gpu-capture`, and the `draft-pr` that a `request_trace` approval
  opens), through one admission check that every mutating tool uses;
- the engine, for `draft-pr` (publish-on-keep) and `workflow-dispatch` (`run_github`),
  through a tally for the run.

Each mediation point enforces the count separately. The broker and the engine each keep a
tally against the declared count, so the bound limits each path, not their sum. A pack with
`draft-pr: 2` allows two publish-on-keep PRs from the engine and two approval PRs from the
broker.

If a write would go over its count or to a target outside its scope, the call fails and names
the bound. The engine adds an `output_refused` session row, and the run continues. Publish
skips the remaining candidates and completes.

**Related hardening.** By default, `jira_add_comment` targets the item that parameterized the
run. `deploy_candidate` validates `image_ref` against the declared registry prefix, and
`codegen_build` validates the push destination of the loop pod against the same prefix.
`workflow_dispatch` targets must be in an operator org allowlist
(`FORGE_GITHUB_ALLOWED_ORGS`). If the allowlist is not set, all targets fail.
`deny_endpoints` denies a host also when a wildcard allow entry would admit it.
`crucible check` warns when a deny entry hides an allow entry that remains.

**Disclosure.** Channels that cannot have an effect type are disclosed instead: resolved
egress (with built-in entries marked), credentials (with whether the value goes into the
agent context and what it authorizes), relay files, broker binary substitution, and whether
the pack runs commands outside the sandbox. `crucible plan exposure` emits the bounds and the
disclosure as versioned JSON, without running pack content. `crucible check` prints them. At
run start, the engine rejects a grant from outside the pack (an agent-visible secret binding)
if the disclosure does not cover its kind and reach.

**Integrating an orchestrator.** An orchestrator uses three commands. All of them run
offline and have no side effects:

- `crucible --contract-version`: before any spend, reject a dispatch target whose version is
  different from yours.
- `crucible plan exposure --manifest <pack>`: the resolved bounds and the capability
  disclosure as versioned JSON, computed without running pack content. Extract it with the
  same pinned engine that you dispatch. Store the document and a digest with the registered
  revision. Compare the digest on every pin bump before you accept it. Show the document
  where a person approves a run, and bind the approval to the digest that you show. For a
  mutable pack, extract the document again from the exact content that you launch.
- `crucible check --manifest <pack>`: the same resolution in a human-readable format, with
  validation findings.

The orchestrator also has two obligations:

- The disclosure must cover each grant that the orchestrator supplies from outside the pack
  (an agent-visible secret, a mounted credential). If it does not, the orchestrator must
  reject the grant before dispatch. The engine also rejects it at run start, so a stale
  stored exposure can fail a launch but cannot widen a run.
- The engine defaults use the environment that the orchestrator sets (`CRUCIBLE_ITEM` for
  the tracker-comment default, the forge variables for image and dispatch targets).

## What it does not do

- **Payloads have no limit.** Bounds control where mediated writes go and how many there
  are. They do not control the content. There is no content sanitization: no domain
  allowlist in a comment body, no mention limits, no dedupe by title, no automatic expiry.
  The reader of the target reviews the payload.
- **No information-flow control.** An `agent_visible` secret and any allowed channel (a
  declared output or a disclosed egress entry) make an exfiltration path. The disclosure
  shows the path to an approver, but nothing stops it at run time. An exposure policy for
  each secret (a secret that declares which output kinds can be present with it) is designed
  but not built.
- **No staged or preview mode.** You cannot do a dry run of the writes of a pack to see what
  it would post.
- **Pack code on the trusted side has the reach of the executor.** Workflow `command` and
  `evaluate` tasks and world and judge hooks run on the loop pod with its credentials,
  outside the sandbox and outside all bounds. The disclosure states this. It does not limit
  it.
- **The rig kubeconfig has full access.** A pack that relays a namespace-`edit` kubeconfig
  into the sandbox grants every write that the role allows, and does not use the typed broker
  tools. The disclosure lists it, but it has no bound.
- **Reads are not included.** `measure`, `profile`, and the fetch tools do not go through
  admission. Bounds apply only to writes.
- **Asks are not outputs.** The operator cap of `C-ASKS` limits work emission. `[outputs]`
  does not.
- **Known enforcement gaps:**
  - `crucible build --check` does not spend or check the dispatch bound, because it
    dispatches nothing. The engine rejects a repository outside the scope only on the real
    dispatch.
  - A standalone `crucible build` outside a run has no session log, so the engine drops its
    rejection row (best-effort, as for the broker).
  - Each mediation point enforces the `draft-pr` count separately. The `request_trace`
    approval PR uses the tally of the broker, and publish-on-keep uses the tally of the
    engine. A pack that declares one PR can get one from each.
  - The `tracker-comment` engine default reads `$CRUCIBLE_ITEM`. An orchestrator exports it
    for runs that a tracker item parameterizes. A run without it gets a default that rejects
    writes.
  - The disclosure of a composite names its own credentials and relays, but it does not
    include the egress or broker reach of its components.
  - An `[agent].env` name without a matching capability declaration causes a
    `crucible check` warning, not an error.
- **The local backend does not change.** `backend = "local"` runs the harness on the host
  with the environment of the operator. No sandbox guarantees apply.

## Comparison with GitHub Agentic Workflows safe outputs

**Common design.** Both have a closed vocabulary of typed output kinds, a maximum and target
limits for each kind declared next to the workflow, conservative defaults, enforcement at a
mediation point that the agent cannot reach, and rejections that do not stop the run. The
GH-AW `target: "triggering"` is the same as the Crucible open-target parameter binding.

**Different structure.** GH-AW makes the agent job fully read-only and sends every effect
through one trusted applier. This works because its workloads are advisory. The Crucible
agent must edit code and run commands, so enforcement is split between the broker and the
engine. Some channels cannot have a type. Crucible handles them with disclosure and human
approval. GH-AW does not need this part and does not have it.

**Additional Crucible features.** The exposure is a stored artifact with a digest. Pin bumps
compare it before acceptance. Approvals bind to the digest that was shown. Launches reject
grants that the disclosure does not cover. Egress and credential reach are in the same
declaration as outputs. GH-AW has no registration or approval process beyond repository
permissions.

**Additional GH-AW features.** Crucible has no equivalent for the payload features of GH-AW:
its sanitization pipeline (body domain allowlists, mention limits, label glob deny-first
boundaries, closing-keyword normalization), its `staged: true` preview mode, and its handler
features (dedupe, expiry, grouping).
