You are tuning a serving-pipeline simulator to minimize its p99 latency. The file `params.py`
has three integer knobs, edit ONLY that file:

- `BATCH_SIZE` (1..64)
- `WORKERS` (1..16)
- `PREFETCH` (0..8)

`sim.py` reads them and reports the simulated p99 (lower is better). There is an interior sweet
spot, not a "crank everything to max" ramp:

- batches too small pay per-request overhead; too large inflate the tail
- too few workers queue; too many contend for a shared resource
- prefetch hides latency up to a point, then just costs bandwidth

{{GOAL}}

Reason about the tradeoffs, change the knobs, and let the gate measure. Each strictly-better
reading is kept (committed); a regression is rolled back, so be bold but read the score.

Current status: {{STATUS}}
{{STEER}}
