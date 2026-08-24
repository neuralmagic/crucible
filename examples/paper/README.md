# paper

The playbook lane against real work: read a speculative-decoding paper and produce an
implementation spec for [vllm-project/speculators](https://github.com/vllm-project/speculators).

Unlike the other examples this one **costs money and calls a real model**. It exists because the
lane's mechanisms are only worth as much as they are on a real task, and every one of them fires
here: a required parameter, a prompt span marked as external input, a skill assembled from a
shipped `SKILL.md`, a declared file captured and committed to git memory, launcher-supplied
ceilings, and a verdict a deterministic command decides rather than the agent.

```sh
crucible plan run --manifest examples/paper/crucible.toml \
  --param paper_url=https://arxiv.org/abs/2503.01840 \
  --max-cost 5 --max-time 20m
```

Measured on EAGLE-3, one turn, $0.81:

```
analyze  pass  cost=$0.8094  out={"algo_name":"eagle3","closest_model":"eagle3","confidence":"high"}
shape    pass  cost=$0.0000  out={"bytes":9025,"has_classification":true,"has_training":true}
plan v1: completed — verdict: valid
```

`crucible plan params --file examples/paper/workflow.star` prints what it accepts without
evaluating a line of it.

## Before you run it

- **It spends real money.** `backend = "local"` runs the `claude` CLI on your machine with
  `--permission-mode bypassPermissions`, so the turn can edit files and run commands inside
  `workspace/`. That directory is a fresh shallow clone, which is the blast radius.
- **It needs network**, for the clone and the paper fetch.
- **It clones a third-party repository** into `examples/paper/workspace/`, which is gitignored.
  Delete it and `state/` to start clean.

## The interesting part is the skill

`skills/analyze-paper/SKILL.md` is the upstream skill from the speculators fork with one change.
Its last step was *"Present to User… ask for approval to proceed"*, written for an interactive
slash command. A playbook turn has nobody to ask and nothing reads what it prints, so that step
is replaced by an output contract: write `SPEC.md`, write `PLAN_TASK_RESULT.json`, do not stop to
confirm anything.

That edit is what porting a hand-rolled skill pipeline into a playbook actually consists of, and
it is the same edit each of the other phases will need.

## What it is not

One phase of six. `implement-speculator` writes code into the checkout and `train-speculator`
wants a GPU; neither is here. `shape` is deliberately a free shell command rather than a second
agent, because a run whose only judge is the agent that did the work has no judge.
