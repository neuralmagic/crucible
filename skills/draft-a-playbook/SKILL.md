---
name: draft-a-playbook
description: Author or change a crucible playbook pack as a draft, in a local working tree, with crux. Use when asked to write, edit, fix or test-fire a playbook pack, a workflow.star, or a crucible draft.
---

# Drafting a playbook pack

A pack lives on the controller as a *draft*: an append-only stack of saves. Each save is
compiled by the controller's pinned engine, and the newest one can be launched. You work on a
copy of it in a directory, with `crux`, and save the directory back.

`crux` needs `CONTROLLER_URL` and `CONTROLLER_API_TOKEN`, or a config file; `crux whoami` says
who the controller takes you for. Every command here is also an MCP tool named `crucible_*` on
the controller's `/mcp`, except pull and push, which read and write your disk.

## The loop

1. **Pull.** `crux draft-pull <draft> pack` writes the newest save into `./pack` and prints its
   version. That version is the base your push carries. For a pack that does not exist yet,
   `crux draft-create <draft> --description '...'` opens one on a skeleton, or on a registered
   pack's files with `--template <playbook>`.
2. **Read before you edit.** `crucible.toml` is the manifest: the agent, the workspace, which
   graph to run. `workflow.star` is the graph. Read both, and the prompts, scripts and schemas
   the tasks you will touch refer to.
3. **Edit the files.** If a language server for `.star` is running (crucible-lsp), it reports
   the engine's compile errors and lints as you edit. Clear what it reports. A deprecation is
   worth fixing only when the request touches that line.
4. **Push.** `crux draft-push <draft> pack --base-version <n>` saves the whole directory as the
   next version and prints its diagnostics. A file you deleted on disk is deleted from the pack.
   `state/`, `workspace/` and `.git` are skipped, and a symlink is refused.
5. **Check the compile.** `crux draft-preview <draft>` prints the schema digest and the
   diagnostics, each with a `file:line:col`. A save that does not compile is still a save: fix
   the anchored lines and push again. A version with no schema digest cannot be launched.
6. **Stop** when the diagnostics are empty and the change does what was asked.

## When a push is refused

Someone else can save between your pull and your push: a person in the studio, or another
agent. The controller then refuses your push, writes nothing, and names the version that
landed.

- Pull that version into a second directory, merge your edits onto its files, and push with
  that version as the base. Their change may be deliberate. Keep it.
- Never retry the same push, and never raise `--base-version` to make the refusal go away.
  Both destroy the other writer's work.

## Test-firing

A launch runs the newest save and costs money, so launch only when the request needs a run to
prove the change, and stay under the ceilings you were given.

```
crux playbook-caps                      # what --max-cost and --max-time may not exceed
crux draft-preview <draft>              # the params schema's digest, and that it compiles
crux draft-launch <draft> --params '{"repo":"o/n"}' --max-cost 1 --max-time 30m
```

Param values are strings. The launch prints its key and a `crucible-playbooks://` link.

To rerun a finished run and reuse what passed, add `--rerun-of <run>`, with `--from <task>` for
each task to run again; everything downstream of those runs again too.

## Following a run

```
crux playbook-run <key>                 # the launch whole: status, cost, its runs and their ids
crux graph <run>                        # each task's latest status, in dependency order
crux run-log <run> -f                   # the engine's output, followed until the run settles
crux turns --issue <key>                # the agent turns; crux turn <pod> shows one in full
crux run-files <run>                    # what the tasks captured, once the run ends
crux run-file <run> '<task>/REPORT.md' --out REPORT.md
```

`graph` shows the latest status per task, not every iteration. Read a failed task's note and
its last log lines before changing anything, then fix the pack, push, and rerun from that task.

## What to read

The crucible docs say what the language allows. An agent image carries them in
`/usr/local/share/crucible-agent/docs`; in the repo they are `docs/`.

- `dsl-reference.md`: every constructor and argument of `workflow.star`.
- `playbooks.md`: a pack built from an empty directory, params, MCP servers, reruns.
- `playbook-patterns.md`: branching on a decision, sending work back, fan-out, joins.

Read the section for the construct you are about to use. Do not guess an argument's name.

## Handing back

Finish with what you changed, file by file, the version you pushed, what the compile said, and
what you ran, if anything, with its launch key and link. Say what you did not verify.

## If a call is refused with 403

Run `crux whoami`. It prints the identity the controller resolved and how you authenticated.
Authoring needs an operator or admin; a read-only identity can list drafts and never save one.
