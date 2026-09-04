# Agent rules

These are enforced in review. A PR that violates them gets bounced regardless of
whether the code works.

## Comments

- No narrative comments. Delete anything that restates the next line, explains
  why a change is correct, tells the design story ("mirrors X", "we do this so
  that"), or references the implementation process. That reasoning belongs in
  the commit message.
- A comment earns its place only by stating a constraint the code cannot show:
  an invariant, a race, a security boundary, an external system's quirk. One
  tight sentence.
- Rustdoc on public items is contract only, one or two sentences. A doc comment
  that restates the item name gets deleted, not reworded. Clap doc comments are
  user-facing `--help` text and are held to the same bar. Module `//!` docs
  state the module's authority and trust boundary, not its history.
- Match the comment density of the surrounding pre-existing code. It is sparse.
- Do not delete or reword pre-existing comments unless the code they describe
  changed.

## Errors

- Typed domain errors are `thiserror` enums. The `#[error]` message names the
  offending value and, where it fits, what would have worked (`OutputsError`
  is the house style). Compile/validation diagnostics carry `file:line:col`
  and a did-you-mean where the vocabulary allows one.
- `anyhow` is for plumbing; every fallible call at a boundary gets `.context()`
  / `.with_context()` naming what was being attempted, lowercase, no period.
- Enforcement paths fail closed. An error on a guard, gate, bound, or policy
  resolution never degrades to "skip the check"; it refuses, and the refusal
  names both the thing refused and the violated bound or missing declaration.
  A refusal that must not kill the run is recorded on the session log and
  returned to the caller as a failed call, not a crash.
- No `unwrap`/`expect`/panic outside tests. Pack-supplied input must never
  abort the engine: bounds are checked before the work they bound.
- No `as` casts that can lose value: widening uses `From`, fallible conversion
  uses `try_from` with the error propagated; fix wire/env/manifest field types
  at the boundary rather than converting at every use.

## Logging and tracing

- `tracing`, structured fields, not string interpolation. `warn` is a survived
  degradation and carries the reason as a field; `error` is operator-action
  rare; `info` marks state transitions.
- Secrets and credentials never reach a log, an error string, a Debug impl,
  argv, or a URL that can end up in an error. The engine's redactor gets every
  redeemed value; keep it that way.
- Stdout is CLI-surface output only; diagnostics go through tracing.

## Types and struct shape

- Strings that encode a closed choice become enums. Identifiers and authorized
  values become newtypes whose only constructor is the check
  (`RepoTarget` via `OrgAllowlist::authorize` is the house style: a checked
  value is a type, so an unchecked one cannot reach the call).
- Put behavior on the domain type that owns the state or specification it acts
  on. Prefer `gate.attempt(...)`, `source.resolve(...)`, and `host.suspend(...)`
  over free functions that take the owner as their first argument; keep a free
  function only when no domain type owns the operation.
- Groups of adjacent scalars in a signature become a struct; mode-dependent
  knobs become an enum keyed by mode. Repeated inline conversions become
  `From`/`TryFrom` impls next to the types.
- No global state (`lazy_static!`, `Once`); thread explicit context structs
  (a run-scoped tally is a field on the run, not a static).
- `crate::` paths, not `super::`.

## Tests

- Unit or end-to-end; no mock objects for internal seams. Faking the far edge
  is fine (a stand-in engine or broker binary on `$PATH`); inventing behavior
  for our own modules is not.
- Fixtures model reality: a fixture pack has a manifest, a stand-in binary
  answers the subcommands the real one has. When enforcement tightens and
  fixtures break, fix the fixture, never soften the guard.
- Test names are sentences. Tests live at the bottom of the module in
  `mod tests {}`.

## Vocabulary

- A scope's human sign-off is an **approval**; never introduce "door" naming.

## PRs, commits, comments

- Never put an AI session link in a PR body, PR description, or commit
  message.
- Agents never post PR comments, review replies, or issue comments; those would
  appear under the operator's account. Report dispositions in the driving
  session instead.
- Commit messages: imperative, under 72 chars on the subject, body says what
  and why. No praise adjectives, no filler.

## Verification

Before handing off engine changes:

- `cargo fmt` and
  `cargo clippy --all --benches --tests --examples --all-features`, zero
  warnings.
- Run the tests you touched, then `cargo test -p crucible --bin crucible`.
  A known flake: `crucible-broker report::tests` races `distress::tests` over
  `SLACK_WEBHOOK_URL`; it passes with `--test-threads=1`.
- `crucible check` against `examples/counter` and `examples/playbook` after
  manifest-surface changes.
- `govctl check` after touching anything under `gov/`.

## Governance

Contracted behavior lives in `gov/` (govctl); RFC-0001/RFC-0002 clauses are the
spec for manifest, egress, outputs, and disclosure semantics. Read the
governing clause before changing enforcement, and file a work item for
follow-ups instead of leaving TODOs in code.
