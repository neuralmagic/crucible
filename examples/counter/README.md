# counter — the crucible litmus domain

The smallest possible domain that exercises the **whole generic engine** with no
cluster and no LLM. If `crucible` can run this, the engine↔domain boundary
([`../../docs/crucible-contract.md`](../../docs/crucible-contract.md)) is real.

- **Repo under test:** a git tree whose entire "code" is an integer in `value.txt`.
- **Judge (`measure.nu`):** score = that integer, `direction = higher`, `solved` at ≥ 5.
- **Proposer (`bump.nu`):** the deterministic `command` agent backend, `value.txt += 1` per
  turn (stands in for an LLM, so the run is free, fast, and reproducible, a real e2e, not a mock).
- **World:** GitWorld (no `[world]` block), kept iterations commit, discards `reset --hard`.

## Run (once the engine exists)

```bash
crucible plan compile-workflow \
  --file examples/counter/workflow.star \
  --manifest examples/counter/crucible.toml

crucible --manifest examples/counter/crucible.toml \
  --graph-loop --iterations 2 --no-early-stop --ui jsonl
```

Expected: `value.txt` ratchets 0→1→2 across kept iterations and the event stream contains
`agent_session` actions `started` and then `resumed`. The private
`state/agent-sessions.json` ledger records `completed_turns: 2` with mode 0600. Increase the
iteration count to reach 5 and stop on `solved`. Swap `backend = "local"` to drive the same
domain with a real Claude Code agent against `method.md`.

## What it proves

Manifest parse → path resolution → `setup_cmd` → propose (`command` backend) → `measure`
contract → universal `decide` (higher-wins) → GitWorld snapshot/restore + commit memory →
Starlark workflow admission → durable session lifecycle → session log + reporters. A real domain
is just this with a heavier judge and a live-rig World.
