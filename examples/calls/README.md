# calls

A playbook that calls another playbook and a GitHub Actions workflow, then folds
both results. Every task is a command, so the parent and child run without a model.

```text
                 ┌─> child  call  calls-child (playbook)        ─┐
  prep ──────────┤     label, config (json), notes (file)        ├─> summary
 (command)       └─> ci     call  $CALLS_DEMO_REPO (github)     ─┘   (command)
                       message, payload (json as string), flavor, loud
```

| Pack | What it shows |
| --- | --- |
| `child/` | `string`, `json` and `file` parameters read from `params` in the inputs JSON; a result task with typed `emits` and one `emits_files` |
| `parent/` | a `calls` block at the top of `workflow.star` declaring `child` (`"kind": "playbook"`) and `ci` (`"kind": "github_workflow"`); `call()` binding a field, a json field and a file of `prep`; `summary` reading both calls' fields and their staged files under `inputs/child/` and `inputs/ci/` |
| `gitlab/` | a `gitlab_pipeline` target: typed `spec:inputs` bound from `prep`, the `report` job's `result.json` fields, and `out/notes.md` staged under `inputs/pipeline/` for `report`; `ci/.gitlab-ci.yml` is the pipeline to push to the project |

The `child` target pins `draft:calls-child`, the draft head, so the parent is a draft pack and the
controller writes `calls.lock` when it saves it. A registered child would be
`playbook = "<registered id>"` and pin its revision digest instead.

The `ci` target names `.github/workflows/crucible-call-demo.yml` on the `crucible-call-demo`
branch of a repository you choose. The sources name it `OWNER/REPO`, and `just calls-example`
writes `CALLS_DEMO_REPO` in its place in the draft it pushes. The workflow takes `message`,
`flavor` (choice), `loud` (boolean) and `payload` (a JSON object as a string), runs `prepare`, a
two-leg matrix `build`, and `report`, and uploads `result.json` and `notes.md` as the
`crucible-result` artifact.

## Run

```sh
just controller-local                          # in one terminal
CALLS_DEMO_REPO=you/demo just calls-example    # in another: saves both drafts, launches calls-parent
```

`CALLS_DEMO_REPO` is required: the owner/name of a repository that has the demo workflow on its
`crucible-call-demo` branch and that your `gh` login can dispatch on.

The local controller dispatches the ci call with the token `gh auth token` prints, held in
memory for the dispatch. It never reaches the run.

`parent/workflow-agent.star` is the same graph with an agent writing the config and notes. The
manifest's `agent` variant selects it and runs a real model under your login.
