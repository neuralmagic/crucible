# loop

The smallest bounded loop. `turn` is an agent turn and `score` is a command; they run in rounds
while `score.go` is true, for at most five rounds. `score.sh` counts rounds in the workspace and
asks for another until the third, so the run takes three of its five rounds.

```
just loop-example
```

The pack's agent is `tools/fake-agent.py` behind the command backend, so only the model is
absent. The recipe runs the pack in a scratch copy.

| | |
| --- | --- |
| rows | `turn[round-1]`, `score[round-1]`, through `turn[round-3]`, `score[round-3]`, then `turn`, `score` and `report` |
| session | `turn` resumes the `work` session in every round |
| input | each round reads `{"round", "max_rounds", "previous"}` under `loop` |
| graph | one `turn` node and one `score` node in a `loop` frame, with one dotted edge back |

`loop` is documented in [Branching and review](../../docs/playbook-patterns.md#loop).
