# route

A playbook that branches on a typed decision. `read` loads a support ticket, `gate` asks a
decision model two questions about it, and each downstream task runs only on the answer it names:

```text
read ──> gate ─┬─ bucket = outage ──> oncall ──┐
               ├─ bucket = billing ─> finance ─┼─> filed   join = "passed"
               ├─ anything else ────> backlog ─┘
               └─ urgent = yes ─────> page
```

## Run

Point the run at any server speaking the System One decision API, then pick a ticket:

```sh
export CRUCIBLE_INFERENCE='{"version":1,"bindings":[{"role":"decision","protocol":"system_one",
  "url":"http://127.0.0.1:8011/v1/systemone","model":"dgemma"}]}'
TICKET=billing crucible plan run --manifest examples/route/crucible.toml --max-cost 1 --max-time 5m
```

Add `"key_env":"SOME_VAR"` to the binding for an endpoint that needs a bearer key held in
`SOME_VAR`. With no decision binding the run truncates at `gate` before spending anything.

## Tickets

| `TICKET` | What it shows |
| --- | --- |
| `outage`, `billing`, `feature` | Clear-cut: one queue, confidently. |
| `torn` | Belongs to two queues and sits near `min_confidence`, so it lands on `finance` or, as `uncertain`, on `backlog`. |
| `vague` | Fits no queue. With no "other" option the model still picks the nearest one, confidently: `min_confidence` does not catch a question that has no right answer. |

## Serve your own model

`serving/` builds vLLM's DiffusionGemma structured-reads server and puts the System One API in
front of it:

```sh
just route-serving-image                 # prints the build job
just route-serving-deploy <image>        # deploy and wait for the model to load
just route-serving-forward               # the endpoint on localhost:8011
```

## Without a model

Swap `min_confidence = 0.8` for `source = read` in `workflow.star` and have `read.sh` emit the
labels itself: the same branches, decided deterministically and for free.

See [Let a decision model answer](../../docs/playbook-patterns.md#let-a-decision-model-answer).
