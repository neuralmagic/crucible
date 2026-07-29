#!/usr/bin/env python3
"""Toy serving-pipeline latency simulator: the python program the agent tunes.

The agent edits the knobs in params.py; this program reads them and reports a simulated p99
latency as the crucible `measure` contract on stdout (last line = JSON). Pure stdlib,
deterministic, fast. There is a real interior optimum, so it's a genuine search, not a ramp:

  - batches too small pay per-request overhead; too large inflate the tail
  - too few workers queue; too many contend for a shared resource
  - prefetch hides latency up to a point, then costs bandwidth

Sweet spot lands near batch~27, workers~9, prefetch~4 (~11 ms) from a ~31 ms start.
"""
import json
import math

TARGET_MS = 13.0  # "solved" when p99 dips under this


def p99_latency(batch: int, workers: int, prefetch: int) -> float:
    base = 5.0
    queueing = 40.0 / workers                       # fewer workers -> more waiting
    contention = 0.4 * max(0, workers - 8) ** 2     # past ~8, workers fight a shared resource
    overhead = 60.0 / batch                         # tiny batches -> fixed per-request cost
    tail = 0.08 * batch                             # huge batches -> head-of-line tail
    prefetch_gain = 6.0 * (1 - math.exp(-prefetch / 2.0))  # saturating benefit
    prefetch_cost = 0.5 * prefetch                  # ... then it just costs bandwidth
    return base + queueing + contention + overhead + tail - prefetch_gain + prefetch_cost


def main() -> None:
    try:
        import params
        batch, workers, prefetch = int(params.BATCH_SIZE), int(params.WORKERS), int(params.PREFETCH)
    except Exception as e:  # the agent wrote something unparseable -> invalid candidate
        print(json.dumps({"valid": False, "note": f"params.py broken: {e}"}))
        return

    if batch < 1 or workers < 1 or prefetch < 0:
        print(json.dumps({"valid": False, "note": "out of range (batch>=1, workers>=1, prefetch>=0)"}))
        return

    p99 = round(p99_latency(batch, workers, prefetch), 2)
    print(json.dumps({
        "valid": True,
        "score": p99,
        "solved": p99 < TARGET_MS,
        "note": f"p99={p99}ms (batch={batch} workers={workers} prefetch={prefetch})",
        "detail": {"batch": batch, "workers": workers, "prefetch": prefetch, "target": TARGET_MS},
    }))


if __name__ == "__main__":
    main()
