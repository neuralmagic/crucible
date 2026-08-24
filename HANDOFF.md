# Playbook lane + launcher: handoff

Date: 2026-08-24. Two repos, one arc: the engine's playbook lane (here, branch
`playbook-lane`) is finished and up as **PR neuralmagic/crucible#63**; the controller
(`~/git/agentic-epp-autoresearch`, branch `playbook-launcher`, ~30 commits, unpushed) grew the
entire launch surface on top of it and is one in-flight workflow leg short of UI pencils-down.
Everything below was adversarially verified by a paired skeptic agent unless marked otherwise.

## Engine (this repo)

All five defect WIs from the previous handoff are closed: lambda counts in the nesting scanner
and `declared_params` is bounded (`26e6609`), markers strip to a fixpoint (`51e431a`), params
bind on one path for every graph spelling (`386d2c1`), declared files publish atomically and
only on pass (`7b69c37` + `44c8799`), and a mapped node's instances run serialized with
per-instance capture (`9991c21`) — suite 986 green. `examples/triage` is the first
real-sandbox example pack (fan-out keyed by issue number, curl-first, no token needed) and is
proven by a fake-agent dry run; its real-model sandbox run is the one open criterion on
WI-2026-08-23-001. Also filed here: WI-2026-08-22-010 (dep-sweep pack, targets wseaton/vllm-vcr,
merge gate default off), WI-2026-08-23-002 (captured-bytes bound must be operator-configurable —
audit Critical vs C-TASK-FILES), WI-2026-08-23-003 (`str(param(...))` launders external text
past the markers; live-reproduced injection, fix = refuse Display-reachable conversions).

**The pin gate:** `core-pin.toml` in the controller points at `7c2c1a5`, which predates the
lane. Registration/preview/dispatch there run against a locally built binary
(`target/release/crucible` here). Production needs: merge #63 → runtime image builds → bump the
controller's core-pin + Cargo revs + lockfile → first boot re-derives stored schemas
(`rederive_stale` was built for exactly this).

## Controller (`~/git/agentic-epp-autoresearch`, branch `playbook-launcher`)

ADR-0027 (accepted) is the design: schema-driven launch surface, one launch path, frozen-at-rev
imports, controller-side dedupe, drafts as the shared human/agent editing surface. Landed, in
rough order:

| what | commits |
| --- | --- |
| Registry (`plan params` at registration, JSONB schema + digest, rederive on pin bump) | `ecb5bab`, hardening `46bf7da` |
| Launch path (`InputKind::Playbook`, adopt-time rows, argv render, field-level 422s, digest-drift 409) | `52fce8d` |
| One-shots (`fire_at`, park-not-poison sweep), cron schedules (SKIP LOCKED claim), cursor dedupe v1 | `9df158c` `eddc25c` `27aa9c2` `7d05cfa` `353a4af` |
| Launch form + schedule toggle + relaunch prefill | `74f2455` `409ed49` |
| Durable imports (`pack_imports` frozen at rev, sharable `/playbooks/import/{id}`, approvals rail) | `aede7f6` |
| Two-way drafts (identity-stamped versions, `base_version` 409-as-merge-prompt) | `27ae90d` |
| MCP front door (21 tools: import/create/files/save; break-glass dev auth) | `23b508e` `4c1b33d` |
| Graph: typed document from `plan compile-workflow` → React Flow, gutter-routed skip-edges, file edge labels | `999add7` `fc384b4` `a2fcf8a` `d2a4ee4` |
| Studio (drafts, compile-on-save, graduation-as-PR), placeholder-bound required params | `4c6840a` `d405444` |
| Local dispatch (`CONTROLLER_PLAYBOOK_EXECUTOR=local`, supervised engine subprocess, launch-time backend refusal) | `63845e0` `3c3c9e6` |
| Playbook run page (journey ladder banished, dispatch first-class), run graph on the shared surface | `5a51583` `b59e0c3` |
| Task evidence on nodes (`/tasks/{task}/evidence`, path-confined incl. bracketed names; `/log`) | `38d7bc4` |
| Monaco everywhere + server-side per-identity editor prefs (`user_prefs`, GET/PUT `/api/prefs/editor`) | `b46ab57` |
| Draft I/O: from-git create (provenance kept), origin + rebase diff (two-way, honestly scoped), tarball, CLI `draft pull/push` | `44f06b3` |
| Native theming (palette drift-tested against global.css) + `@headless-tree` file tree | `be16bc6` |
| Resizable panes (`react-resizable-panels`), editor fills pane, collapsible icon rail | `4b9dcb8` |
| Molten masthead logo (WebGPU port of `just demo-logo`, 30px, transparent ground, static fallback) | `133ee7b` |
| Fixes: upstream-filter `$8/$9` bind collision (mine, verifier-caught), operator authoring gates | `02720fc` `3639c89` |

**Nothing in flight.** The polish wave closed complete: WI-011 landed as `61908df` (one
`CoDraft` renderer feeds both the served SKILL.md and the hint commands, so human and agent
can never see different URLs; break-glass lines render only on a local deployment) and the
invisible DOWNLOAD SKILL label it exposed was a pre-existing unlayered `a{color:inherit}`
beating Tailwind utilities — fixed by layering the rule (`global.css`, own commit). UI is
pencils-down by decree.

**Proven live on this laptop:** the whole loop. An opus agent authored "fences" (a Chesterton's
Fence archaeology pack, draft v7) through the MCP tools; a human edit landed mid-loop and the
agent surfaced the stale-base refusal instead of clobbering; the pack ran locally
($0.67, `pallets/itsdangerous`, verdict `keep` — correct); `triage-local` (templated from the
registered pack, backend flipped to local in the studio) proved 4-instance fan-out against real
speculators issues. Runs live under `state/local-runs/`. The agent e2e is committed:
`MCP_AGENT_E2E=1 just mcp-agent-e2e`.

## Open work, ranked

2. **Demo kit** (promised, not started): `just demo` seeded boot; a Playwright-recorded
   talk-over video — scene list agreed: registry → MCP import hardlink → studio co-edit +
   stale-base diff → local launch → fan-out graph → evidence panel → schedule toggle; a
   walkthrough artifact with the session's screenshots.
3. **Merge train**: push + PR the controller branch; merge crucible #63; pin bump; deploy.
4. Small reds on the branch: two `PackDispatchNotice` Playwright specs (import.spec.ts:89,
   launch.spec.ts:130); merge-diff caption says left/right while Monaco renders inline under
   900px panes; `design.spec.ts` baselines stale (own WI-worthy chore); `engine.log` mirror in
   `local_run.rs` never exercised by a fresh run; audit W1: `register()` doesn't refuse a live
   draft's id (drafts refuse registered ids, not vice versa).
5. Queued WIs: controller — secrets (WI-2026-08-23-006, ADR-gated; **another session** is
   drafting ADR-0028/0029/0030 as untracked files in gov/adr — leave them alone),
   ADR-0026 phase 5; crucible — dep-sweep pack, captured-bytes knob, laundering fix,
   WI-2026-08-22-006 v2 seen-set (cursor write-back hole noted on the WI).

## Decisions made, do not relitigate

- The pack declares (`params`, secrets-by-name someday); the controller renders and enforces,
  never authors a registered pack. One registration path, one launch path, one ledger.
- Imports and launches are **frozen at rev**; the link shows what was proposed even after the
  branch moves.
- Agents **propose** (operator), humans **register/launch/delete/graduate** (admin); draft
  authoring is operator-level (`3639c89`).
- Dedupe is the launcher's: cursor first, seen-set later; one-shots never advance it unless
  opted in. One-shots are not degenerate cron.
- Playbook rows stay off the autoresearch surfaces (issues board, inbox, runs leaderboard via
  `kind=`/`exclude_kind=`); they have their own rail and run page; the journey ladder never
  renders for them.
- Editor prefs are **server-side per identity**; pane sizes and rail state are **per-device
  localStorage** (a laptop and an ultrawide must not fight).
- Editor is **Monaco** (CodeMirror was reversed before a line landed), tree is
  `@headless-tree`, splitters are `react-resizable-panels` — all exact-pinned after
  verify-current checks.
- Drafts have no git remote of their own: git enters via import, leaves via graduation.

## Dev-stack runbook (the traps are real)

- Postgres: existing container `crucible-test-pg`, `postgres://postgres:ci@localhost:55432/crucible`;
  `.env` at workspace root (git-excluded locally) feeds sqlx macros; `justfile.local` exports
  DATABASE_URL for recipes. After migrations: `sqlx migrate run --source crucible-controller/migrations`.
- Daemon: `DATABASE_URL=... CRUCIBLE_BIN=~/git/crucible/target/release/crucible
  CONTROLLER_ADMINS=dev,wseaton CONTROLLER_API_ADDR=127.0.0.1:8899
  CONTROLLER_PLAYBOOK_EXECUTOR=local ./target/release/crucible-controller autopilot`.
  **The docs' `CONTROLLER_ADMINS=$USER` is a trap**: the browser identity is `dev` (vite proxy
  injects it); omit it and every admin surface reads viewer/read-only.
- SPA: `bun run dev` in `crucible-controller/ui` → localhost:5173.
- MCP against the dev daemon: break-glass needs `CONTROLLER_BREAK_GLASS=true` (clap rejects
  `1`) **and** `CONTROLLER_API_TOKEN` set to anything. Headless `claude -p` needs
  `--permission-mode bypassPermissions` and `< /dev/null`.
- A draft save is the **full tree** — a file missing from the map is deleted.
- WebGPU renders black in headless screenshots (capture, not code); prove visuals in a real
  browser. `moltenSim.ts`/`MoltenLogo.tsx` are named to dodge APFS case-collision.
- Flakes: "unexpected response from SSLRequest" under full-suite load (re-run);
  `bun test` ≠ `bun run test` (use the latter);
  `otel::forwarding_mirrors_reparented_traces...` (crucible repo, not ours).

## How this was built

Serial one-implementer workflows (opus) with a paired adversarial verifier per task and up to
two repair rounds, redirected mid-flight at task boundaries via stop → edit script → resume
(cache replays finished legs). The verifiers earned their keep repeatedly: the batched-staging
bug, the register/discard race, the fan-out cost double-count, the `$8` bind collision, the
missing `crucible_draft_create` (found by the agent-in-the-loop e2e's opus leg, which refused
to hand over a 404 link and filed a gap report instead). Trust reports that say *observed*,
not *reasoned*.
