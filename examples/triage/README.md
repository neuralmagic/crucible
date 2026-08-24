# triage

Workflow automation in the playbook lane: point it at a GitHub repository and it triages
the newest open issues. A `scan` agent discovers the issues, `over = scan.issues` fans
out one **isolated triage instance per issue** inside a real sandbox image, and a free
deterministic command assembles `REPORT.md` from what the passing instances captured.

```sh
crucible plan run --manifest examples/triage/crucible.toml \
  --param repo=vllm-project/speculators \
  --max-cost 5 --max-time 20m
```

`--param label=bug` narrows the sweep, `--param limit=10` widens it (`max_fanout` caps it
at 12 regardless). `crucible plan params --file examples/triage/workflow.star` prints the
accepted parameters without evaluating a line of the graph.

## What it exercises

Everything the lane has, on one small graph:

- **Runtime fan-out**: `scan` emits the issue list and `triage` maps over it, keyed by
  issue number, so instance `triage[123]` means issue 123 on every retry and report row.
- **A real sandbox**: `backend = "openshell"` runs each agent turn in the stock
  claude-sandbox image, deny-by-default egress. The public GitHub API is reachable
  because the built-in allowlist carries the forges; `curl`/`gh` are listed as egress
  binaries in the manifest.
- **Only passing instances feed the join**: an issue whose fetch fails is a failed
  instance and a missing row, not an invented one. `required = False` keeps a partial
  sweep alive.
- **Declared files through isolation**: each instance's `TRIAGE.md` is captured from its
  isolated workspace and staged read-only under `inputs/triage[<n>]/` for the roundup.
- **A verdict no agent grades**: `roundup` is python over captured files and
  `CRUCIBLE_INPUTS`; the report is assembled from evidence, not from an agent's summary
  of its own work.

## Before you run it

- **It spends real money**, one scan turn plus one turn per issue.
- **It needs no GitHub token** for public repositories: the skills read the
  unauthenticated public API (60 requests/hour per IP). With `GH_TOKEN` set and `gh` in
  the image, the authenticated path is used instead.
- Nothing is posted to GitHub. The draft responses land in `REPORT.md`; posting them is
  the operator's call.

## Where it goes

The controller adopts GitHub inputs and launches one run per adopted issue; this pack is
the other direction, one run sweeping many issues. The parameters are the boundary: an
orchestrator that can answer `plan params` can launch this pack with no knowledge of what
is inside it.
