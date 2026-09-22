# revise-loop

The smallest bounded revise loop, run with no model.

`author` is an agent turn that drafts `PROBE.md`. `review` is a command that rejects a draft
written without a verdict in hand and accepts the revision written with it, so the run takes two
of its three rounds. The pack's `claude` is `tools/fake-agent.py` behind Claude Code's argv,
stdin prompt, session flags, transcript, and stream-json result. The container, session, upload,
and download paths are therefore the real ones; only the model is absent.

```
just revise-loop-e2e
```

Needs podman (a running `podman machine` on macOS), `openshell`, and `openshell-gateway`. The
recipe builds `localhost/crucible-fake-claude:dev` from `image/`, runs the pack in a scratch
copy, and asserts what the loop owes:

| | |
| --- | --- |
| rows | `author[round-1]` pass, `review[round-1]` fail, `author[round-2]` pass, `review[round-2]` pass, then `author` and `review` |
| session | `workspace/SESSION.log` reads `start` then `resume`: the second round resumed the first round's conversation |
| revision | the second draft carries the reviewer's verdict, and the reviewer's rejected evidence was staged under `inputs/review/` |
| git memory | two `task author` commits, one per passing round |

`revise` is documented in [Work graphs](../../docs/work-graphs.md).
