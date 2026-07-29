# tuner — the crucible demo domain

A real agent tunes the knobs of a python **serving-pipeline simulator** to cut its p99 latency.
This is the EPP story (lower a latency metric by tuning parameters) with no cluster, no GPU,
pure stdlib, so you can run and watch it anywhere. The deterministic `examples/counter/` proves
the plumbing; **this** one is the satisfying demo.

- **Program under test:** `sim.py`, a toy latency model with a genuine interior optimum.
- **Knobs (`params.py`):** `BATCH_SIZE`, `WORKERS`, `PREFETCH`, the only thing the agent edits.
- **Judge:** `python3 sim.py` emits `{valid, score, solved}`; `direction = lower`; `solved`
  when p99 dips under 13 ms.
- **World:** GitWorld, each strictly-better reading is committed, regressions reset --hard.

It's a real search: from the `8 / 2 / 1` start (~31 ms) the optimum sits near `27 / 9 / 4`
(~11 ms), and cranking everything to max makes it *worse* (~37 ms), so the agent has to reason
about the tradeoffs, not just ramp.

## Run

```bash
crucible --manifest examples/tuner/crucible.toml --iterations 8
```

Watch p99 fall across kept iterations until it crosses 13 ms and the loop reports `solved`.
(Uses the `local` agent backend, a real Claude turn, so it needs the Vertex creds in
`[agent].env`. For a free, deterministic plumbing check, run `examples/counter/` instead.)
