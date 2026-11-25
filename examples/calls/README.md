# calls

A playbook that calls another playbook and a GitHub Actions workflow (RFC-0008), then folds
both results. Every task is a command, so the parent and child run without a model.

```text
                 ┌─> child  call  calls-child (playbook)        ─┐
  prep ──────────┤     label, config (json), notes (file)        ├─> summary
 (command)       └─> ci     call  wseaton/snapshotd (github)    ─┘   (command)
                       message, payload (json as string), flavor, loud
```

| Pack | What it shows |
| --- | --- |
| `child/` | `string`, `json` and `file` parameters read from `params` in the inputs JSON; a result task with typed `emits` and one `emits_files` |
| `parent/` | a `calls` block at the top of `workflow.star` declaring `child` (`"kind": "playbook"`) and `ci` (`"kind": "github_workflow"`); `call()` binding a field, a json field and a file of `prep`; `summary` reading both calls' fields and their staged files under `inputs/child/` and `inputs/ci/` |

The `child` target pins `draft:calls-child`, the draft head, so the parent is a draft pack and the
controller writes `calls.lock` when it saves it. A registered child would be
`playbook = "<registered id>"` and pin its revision digest instead.

The `ci` target names `.github/workflows/crucible-call-demo.yml` on the `crucible-call-demo`
branch of `wseaton/snapshotd`. It takes `message`, `flavor` (choice), `loud` (boolean) and
`payload` (a JSON object as a string), runs `prepare`, a two-leg matrix `build`, and `report`,
and uploads `result.json` and `notes.md` as the `crucible-result` artifact.

## Run

```sh
just controller-local          # in one terminal
just calls-example             # in another: saves both drafts, launches calls-parent
```

The local controller dispatches the ci call with the token `gh auth token` prints, held in
memory for the dispatch. It never reaches the run.

`parent/workflow-agent.star` is the same graph with an agent writing the config and notes. The
manifest's `agent` variant selects it and runs a real model under your login.
