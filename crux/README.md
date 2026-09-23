# crux

The crucible controller as a CLI, over its HTTP API.

It exists to replace `curl | jq` against the controller. Every operation is a subcommand that
encodes issue keys for you, carries the controller's error body through verbatim, and prints a
line where the API prints kilobytes. The same operations are the MCP tools the controller hosts
at `/mcp`: same `ops` code, same bytes, so an agent can loop over `crux issues` in a shell when
that is cheaper than a tool call per issue and get exactly what the tool would have returned.

The controller hosts the same tools at `/mcp`. **This file is the single source for the flags,
environment variables, and config file below.**

```
cargo install --locked --git https://github.com/neuralmagic/crucible.git crux
export CONTROLLER_URL=https://crucible-api.apps.int.spoke.prod.us-east-1.aws.paas.example.com
export CONTROLLER_API_TOKEN=crk_...
crux whoami
```

## Authentication

One credential: the bearer in `CONTROLLER_API_TOKEN`.

- **A minted key** (`crk_…`), minted on the controller's Settings page, authenticates as the person
  who minted it, with the groups their last browser sign-in recorded. Mutations are booked to
  them. This is the credential to use.
- **The static controller bearer** authenticates as `anonymous`, or as the identity the deployment
  pins to it (`CONTROLLER_API_TOKEN_IDENTITY`). Reads work; every admin/operator route 403s.
  `crux whoami` names it plainly, and a 403 on that path carries a hint saying so.

Nothing in this tool names an identity. The controller resolves the key, and a caller who wants
to act as somebody mints a key as that somebody.

## Configuration

Every setting is a flag, an env var, and a config-file key. A flag or env var wins over the file;
the file exists so that nothing has to be exported.

| Env | Flag | File key | Meaning |
| --- | --- | --- | --- |
| `CONTROLLER_URL` | `--url` | `url` | Controller base URL: the API route, not the browser route. Defaults to `http://127.0.0.1:8870`, a controller on this machine. |
| `CONTROLLER_API_TOKEN` | `--api-token` | `api_token` | A minted `crk_` key, or the static controller bearer. |
| `CONTROLLER_CONFIG` | `--config` | — | The config file to read. Defaults to `config.toml` under `$XDG_CONFIG_HOME/crux` (else `~/.config/crux`), which may be absent; a path given here must exist. |

```toml
# ~/.config/crux/config.toml (chmod 600)
url = "https://crucible-api.apps.int.spoke.prod.us-east-1.aws.paas.example.com"
api_token = "crk_..."
```

Unknown keys are refused, so a typo is an error and not a silently ignored line.

## Output

Compact plain text, built to be read in an agent's context. `--json` returns the controller's
payload verbatim instead. Errors carry the HTTP status and the API's error body **exactly as
written** — the controller's 422 on a bad adopt names the field that failed, and that sentence is
the whole value of the response.

## Operations

`crux --help` is the full list. Each subcommand is also an MCP tool (`crucible_*`) on the hosted
surface. The ones worth knowing before the rest:

### `whoami`

```
$ crux whoami
endpoint: https://crucible-api.example.com (configured CONTROLLER_URL)
credential: api key
user: wren
role: admin
admin: true
groups: /groups/crucible
controller auth mode: native
```

The first command to run when a mutation 403s. `endpoint` is where the requests went, `credential`
is what they carried, and the rest is who the controller says that makes you; the interesting case
is when they disagree.

### `issues` — the backlog, one line each

```
$ crux issues --kind scenario --status awaiting-approval
2 issues
KEY                                            STATUS             TIER  REF     CONTRACT     UPSTREAM       TITLE
owner/repo#12                                  awaiting-approval  T1    -       -            owner/repo#12  Cache-aware request routing
scenario:0192f4a1-8c3e-7000-9abc-1234567890ab  awaiting-approval  T1    nv_dev  epp-measure  -              Adopt the EPP calibration scenario
```

Scenario issues have no upstream, so that column is a dash — which is the fact you want visible,
since it means there is no GitHub thread to go read.

### `issue <key>` — one issue in full

```
$ crux issue 'owner/repo#12'
```

Provenance, the **untruncated** park reason, every scope with its approval PR and pack digest, each
run and candidate, and the state-transition trail with actors. Keys carry `/` and `#`; encoding is
handled for you, so paste them raw (quoted against your shell).

### `adopt` — create a scenario issue with no upstream

```
$ crux adopt --title "Calibrate the EPP scorer" \
    --body ./ask.md \
    --repo neuralmagic/crucible --repo llm-d/epp \
    --justification "blocks the Q3 routing measurement" \
    --git-ref nv_dev --codegen-contract epp-measure
adopt scenario:0192f4a1-8c3e-7000-9abc-1234567890ab: accepted
tier: T1
repos: neuralmagic/crucible, llm-d/epp
git_ref: nv_dev
codegen_contract: epp-measure
actor: wren
```

`--body -` reads stdin. The first `--repo` is the clone target. The ack echoes what the controller
*stored*, not what was sent: it trims and validates both, and a silently-normalized ref is worth
seeing before a turn clones the wrong branch.

### `park` / `unpark` / `bump` / `redispatch` / `reconcile`

```
$ crux park 'owner/repo#12' --reason "flaky measure harness, see ACME-1234"
$ crux unpark 'owner/repo#12' --reason "harness fixed"
$ crux bump 'owner/repo#12' --priority 200
$ crux redispatch 'owner/repo#12' --justification "the first run OOMed on the judge"
$ crux reconcile
```

`reconcile` takes **no key** — the controller's endpoint is a full pass, it returns before the pass
runs, and repeated calls coalesce.

### `playbooks` / `playbook-caps` / `playbook-schema` / `launch` / `playbook-runs`

```
$ crux playbook-caps                       # the ceilings --max-cost and --max-time may not exceed
$ crux playbook-schema survey              # the params object a launch is validated against
$ crux launch survey --params '{"topic":"attention sinks"}' --max-cost 4 --max-time 30m
$ crux playbook-runs --status running
```

Read the schema before filling one in; guessing the field names is how a launch turns into a 422.

### `runs` / `run` / `graph` / `run-log` / `run-files` / `run-file`

```
$ crux graph 0192run                       # each task's latest status, in dependency order
$ crux run-log 0192run -f                  # follow the engine's output until the run settles
$ crux run-log 0192run --cursor 200        # one window, resumable by cursor
$ crux run-files 0192run                   # what the tasks captured, once the run ends
$ crux run-file 0192run 'judge/REPORT.md' --out REPORT.md
```

`graph` prints the **latest** status per task rather than every iteration — a loop that ran twelve
times would otherwise print twelve copies of the same verdict. `(any)` means the task runs when
*one* dependency passes, so a red dependency above it is not necessarily what is blocking it.
`--mermaid` emits a flowchart.

### Drafts: `draft-create` / `draft-pull` / `draft-push` / `draft-preview` / `draft-launch` / `draft-graduate` / `draft-delete`

```
$ crux draft-create calibrate --description "EPP calibration sweep"   # version 1, a skeleton
$ crux draft-pull calibrate ./calibrate                               # write the pack into a dir
$ crux draft-push calibrate ./calibrate --base-version 1              # save the dir as version 2
$ crux draft-preview calibrate                                        # compile: digest + diagnostics
$ crux draft-launch calibrate --params '{"repo":"o/n"}' --max-cost 4 --max-time 30m
$ crux draft-graduate calibrate --repo wren/packs --path packs/calibrate
```

A push carries the version it edited from, and a save that another editor overtook comes back as
the refusal naming the version that landed since, not as an error: re-pull, merge, push again.
`skills/draft-a-playbook/SKILL.md` is the full authoring workflow.

### `secrets` / `secret-bind`

```
$ crux secrets                                   # id, name, kind, visibility, mode, owner — never a value
$ crux secret-bind 01a0 --playbook calibrate --env GH_TOKEN --declared-name pr-token
```

A draft's launch is refused until every `[[secret]]` its manifest declares is bound at its scope
by someone who owns the secret.

### `approve <key>` / `approval-detail <key>` / `approvals`

`approve` works for **scenario and jira** issues. A GitHub issue has no such endpoint: its approval
is a draft PR that a human approves on GitHub, which the controller polls; `approval-detail` prints
that PR URL plus the pack digest under it, and whether anyone has approved. `approvals` lists every
scope currently awaiting a human, plus the PRs already kept.

### `deployed`

```
$ crux deployed --expect $(git rev-parse HEAD)
```

Which build the controller is running. With `--expect` it is a gate: exits non-zero when the commit
named is not the one answering, so a deploy step can stop instead of reporting success it did not
verify.
