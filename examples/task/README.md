# task

The no-judge lane's litmus domain: a manifest with no `[judge]`.

The loop runs exactly as ever (sandbox, session log, publish, resume), but with the built-in
keep-everything `TaskJudge`: no baseline measure, no score, every completed turn is kept and
snapshotted, and the run exits 0 when the iterations are spent. Use it for unsupervised chores
("consolidate the open dependabot PRs", "fix flaky tests nightly") where the deliverable is the
kept commits — published as a draft PR when `[publish].pr_repo` is set — not a number.

```sh
crucible --manifest examples/task/crucible.toml --iterations 3
```

Expected: exit 0, three `keep` rows with no score in `state/session.jsonl`, and three kept
commits in `workspace/` on top of the baseline.

The deterministic `command` backend (`chore.sh`) stands in for an LLM; a real task swaps in
`backend = "local"` or `"openshell"` with a goal prompt. This manifest is also the reference
shape for what a controller-authored task pack looks like: `[repo]` + `[agent]`, nothing else
required.
