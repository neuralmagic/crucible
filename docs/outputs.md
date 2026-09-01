# Bounded outputs: current state

What the `[outputs]` and capability-disclosure machinery does today, what it
deliberately does not do, and how it compares to GitHub Agentic Workflows'
safe outputs. The normative spec is RFC-0001 `C-OUTPUTS` and
`C-CAPABILITY-DISCLOSURE`; this page is the honest operational summary, plus
the contract an orchestrator integrates against.

## The mental model

An agent never touches the outside world directly. Every side effect leaves a
run through one of two doors: a broker tool the agent asks for, or an action
the engine itself performs (publishing a kept candidate, dispatching a build).
`[outputs]` is the pack declaring, before anything runs, how many writes of
each kind its runs may make and where they may land. Both doors check that
ledger before acting; a write over its count or off its target fails with the
reason named and the run continues. What cannot be counted that way — a raw
credential, network reach — cannot be bounded, so it must be declared instead:
that declaration is the exposure, the approving human reads it, and a grant
that is not on it refuses at launch. Firewall rules for agent side effects,
plus a customs declaration checked at the border.

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

**Declaration.** A pack may declare, per output kind, a per-run count and a
target in `[outputs]`. The vocabulary is closed and engine-versioned:
`draft-pr`, `tracker-comment`, `chat-message`, `image-push`, `deploy`,
`workflow-dispatch`, `gpu-capture`. A kind outside the vocabulary, a
declaration without a count, an unbounded target (`*`, `**` — fixed or open),
or an any-target scope is a manifest error. An open target names a scope and
may bind to a workflow parameter, in which case the write's target must equal
that parameter's run value. Undeclared kinds resolve from a documented engine
default table: every default carries a count, none carries an open target.

**Enforcement.** Bounds load from the frozen manifest and are enforced at two
mediation points the agent cannot reach:

- the broker, for the tool-written kinds (`tracker-comment`, `chat-message`,
  `image-push`, `deploy`, `gpu-capture`, and the `draft-pr` a `request_trace`
  approval opens), through one admission check every mutating tool consults;
- the engine, for `draft-pr` (publish-on-keep) and `workflow-dispatch`
  (`run_github`), through a run-scoped tally.

A count is enforced per mediation point: the broker and the engine each keep
their own tally against the declared count, so the bound caps each path rather
than their sum. A `draft-pr: 2` pack allows two publish-on-keep PRs from the
engine and two approval PRs from the broker.

A write that would exceed its count or address a target outside its scope
fails the requesting call naming the violated bound, appends an
`output_refused` session row, and does not terminate the run. Publish skips
the remaining candidates and completes.

**Adjacent hardening shipped with it.** `jira_add_comment` defaults its target
to the item that parameterized the run. `deploy_candidate` validates
`image_ref` against the declared registry prefix, and `codegen_build` validates
the loop pod's push destination against it the same way. `workflow_dispatch`
targets must clear a fail-closed operator org allowlist
(`FORGE_GITHUB_ALLOWED_ORGS`; unset refuses everything). `deny_endpoints`
denies a host even when a wildcard allow entry would readmit it, and
`crucible check` warns on denies a surviving entry still shadows.

**Disclosure.** Channels that cannot be effect-typed are disclosed instead:
resolved egress (built-ins marked as such), credentials with whether their
value enters agent context and what they authorize, relay materializations,
broker binary substitution, and whether the pack runs commands outside the
sandbox. `crucible plan exposure` emits bounds + disclosure as versioned JSON
without executing pack content; `crucible check` prints them. A grant from
outside the pack (an agent-visible secret binding) whose kind and reach the
disclosure does not cover is refused at run start.

**Integrating an orchestrator.** The engine's whole integration surface is
three invocations, all offline and side-effect free:

- `crucible --contract-version` — refuse a dispatch target whose version is
  not yours before any spend.
- `crucible plan exposure --manifest <pack>` — the resolved bounds plus
  capability disclosure as versioned JSON, computed without executing pack
  content. Extract it with the same pinned engine you dispatch, store the
  document and a digest with the registered revision, diff the digest on every
  pin bump before accepting it, and present it wherever a human approves a
  run, binding the approval to the digest presented. A mutable pack must be
  re-extracted from the exact content launched.
- `crucible check --manifest <pack>` — the same resolution printed for humans,
  plus validation findings.

Two obligations run the other way: a grant the orchestrator supplies from
outside the pack (an agent-visible secret, a mounted credential) must be
covered by the disclosure or refused before dispatch — the engine
independently enforces the same refusal at run start, so a stale stored
exposure can fail a launch but never widen a run — and the environment the
orchestrator projects is what the engine defaults resolve from
(`CRUCIBLE_ITEM` for the tracker-comment default, the forge variables for
image and dispatch targets).

## What it does not do

Read this list before trusting the machinery with something it does not cover.

- **Payloads are unbounded.** Bounds govern where mediated writes land and how
  many, never what they say. There is no content sanitization: no domain
  allowlist inside a comment body, no mention caps, no dedupe-by-title, no
  auto-expiry. The reader on the addressed target is the payload's review.
- **No information-flow control.** An `agent_visible` secret plus any allowed
  channel — a declared output or a disclosed egress entry — is an exfiltration
  path. Disclosure makes the path visible to an approver; nothing stops it at
  run time. Per-secret exposure policy (a secret forward-declaring which
  output kinds it tolerates co-residing with) is designed but not built.
- **No staged/preview mode.** There is no way to dry-run a pack's writes and
  see what would have been posted.
- **Pack code on the trusted side keeps the executor's reach.** Workflow
  `command`/`evaluate` tasks and world/judge hooks run on the loop pod with
  its credentials, outside the sandbox and outside every bound. The
  disclosure states that this happens; it does not narrow it.
- **The rig kubeconfig is undiminished.** A pack that relays a namespace-`edit`
  kubeconfig into the sandbox grants every write that role allows, bypassing
  the typed broker tools. It appears in the disclosure; it is not bounded.
- **Reads are out of scope.** `measure`, `profile`, and the fetch tools are
  not routed through admission; bounds cover writes only.
- **Asks are not outputs.** Work emission is bounded by `C-ASKS`'s own
  operator-configured cap, not by `[outputs]`.
- **Known enforcement gaps, tracked as work items:**
  - `crucible build --check` neither spends nor checks the dispatch bound
    (it dispatches nothing); an out-of-scope repo is refused only on the real
    dispatch.
  - a standalone `crucible build` outside a run has no session log, so its
    refusal row is dropped (best-effort, like the broker's).
  - the `draft-pr` count is enforced per mediation point: `request_trace`'s
    approval PR spends the broker's tally, publish-on-keep spends the
    engine's, and a pack declaring one PR can therefore see one from each.
  - the `tracker-comment` engine default reads `$CRUCIBLE_ITEM`; an
    orchestrator exports it for runs parameterized by a tracker item, and a
    run launched without one gets a default that refuses writes rather than
    scoping them.
  - a composite's disclosure names its own credentials and relays but does
    not fold in its components' egress or broker reach.
  - an `[agent].env` name with no matching capability declaration is a
    `crucible check` warning, not an error.
- **The local backend is unchanged.** `backend = "local"` runs the harness on
  the host with the operator's environment; none of the sandbox-tier
  guarantees apply there.

## Comparison with GitHub Agentic Workflows safe outputs

Walters' argument (Agentic AI and software forges, 2026) is that GH-AW is the
minimum quality bar and anything similar should publish a comparison. This is
ours.

Same core: a closed vocabulary of typed output kinds, per-kind max and target
constraints declared next to the workflow, conservative auto-defaults,
enforcement at a mediation point the agent cannot reach, refusals that do not
kill the run. Their `target: "triggering"` is our open-target parameter
binding.

Structurally different: GH-AW makes the agent job fully read-only and brokers
every effect through one trusted applier, which works because its workloads
are advisory. Crucible's agent must edit code and execute things, so
enforcement splits across the broker and the engine, and a class of channels
survives that cannot be typed at all — those are handled by disclosure plus
human approval, a half GH-AW does not need and does not have.

Where crucible goes further: exposure is a stored, digest-tracked artifact;
pin bumps diff it before acceptance; approvals bind to the digest presented;
launches refuse grants the disclosure does not cover; egress and credential
reach are part of the same declaration as outputs. GH-AW has no registration
or approval story beyond repository permissions.

Where GH-AW remains ahead: everything in "what it does not do" about payloads
— its sanitization pipeline (body domain allowlists, mention limits, label
glob deny-first boundaries, closing-keyword normalization), its `staged: true`
preview mode, and its per-handler ergonomics (dedupe, expiry, grouping) have
no crucible equivalent today.
