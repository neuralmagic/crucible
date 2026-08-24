---
name: triage-fips
description: Explain why one build variant stopped resolving crypto to the system OpenSSL, and propose the minimal fix.
---

# Triage a FIPS blocker

A deterministic probe has already decided this variant is dirty. Your job is the part a rule
cannot do: say **which dependency edge** put the blocker in the compiled graph, propose the
**smallest change** that removes it without breaking the other variants, and draft the tracking
issue for the fork that has not synced yet.

You are given `repo` (the watched repository) and `downstream_repo` (the fork that inherits this
code on the next sync). The variant you are triaging is your task's item.

## What you have

- `inputs/probe[<variant>]/PROBE.md` — the probe's finding: the blocker crates and the head of
  each `cargo tree -i` path.
- `checkout/` — the repository at the watched commit.
- `variants.json` — every variant's package, target and feature set.

## Step 1: Find the edge, not just the crate

Read the manifests. `cargo tree -e normal -i <blocker> -p <package> --target <target>` with the
variant's feature flags reproduces the probe. Two questions decide the fix:

- Which feature enables the dependency that carries the blocker? A feature named for storage or
  auth often enables a TLS backend four levels down.
- Is the enabling **unconditional** (a plain feature list on a workspace dependency) or **gated**
  behind a feature the FIPS variant does not select? Unconditional is the common cause and the
  easy fix; gated means the variant selects it deliberately and the fix is larger.

Beware two traps that make a wrong answer look right:

- **The lockfile is not the build graph.** A crate in `Cargo.lock` may be an optional dependency
  nothing enables. Only `cargo tree -e normal` counts.
- **The host target is not the shipped target.** `native-tls` resolves to Security.framework on
  macOS and to OpenSSL on Linux. Always pass the variant's `--target`.

## Step 2: Propose the smallest fix, and check it

Prefer moving a feature onto the path that needs it over removing functionality. Then **verify**:
re-run the probe's `cargo tree` with your change applied and confirm the blocker is gone from the
dirty variant AND still present (unchanged) for the variants that legitimately use it. Revert the
checkout afterwards. If you cannot make the blocker disappear without breaking another variant,
say so plainly — an honest "this needs a real decision" beats a fix that moves the problem.

## Step 3: Write your output

Nothing you print is read. Three files are your entire output:

1. `TRIAGE.md` — the edge, the root cause, the proposed diff, and what you verified. Say
   "verified" only for a `cargo tree` you actually ran.
2. `ISSUE.json` in the workspace root — exactly:

   {"repo": "<downstream_repo>", "title": "...", "body": "...", "dedupe_key": "<variant>:<blocker>"}

   The title names the variant and the blocker. The body explains the edge, the proposed fix, and
   why it matters for the fork — it inherits this on the next sync. The `dedupe_key` must be
   stable across runs so a schedule updates one issue instead of opening a new one each firing.
3. `PLAN_TASK_RESULT.json` — exactly:

   {"blocker": "<crate>", "root_cause": "<one line>", "confidence": "high|medium|low"}

Write all three before you finish. Do not ask for approval and do not stop to confirm anything.
