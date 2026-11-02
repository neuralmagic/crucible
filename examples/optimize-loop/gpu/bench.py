"""The GPU benchmark harness. The verifier pins this file: edit kernel.py instead.

It builds fixed attention inputs on the GPU, checks `kernel.attention` against a float64
reference computed here, times it, and prints one line:

    optimize-loop-result {"checksum": ..., "ops_per_s": ..., "p50_ms": ..., ...}

The checksum is a hash of the reference output, printed only when the kernel matched it, so every
correct variant prints the baseline's checksum and an incorrect one prints "mismatch".
"""

import hashlib
import json
import os
import socket
import statistics
import time

import torch

import kernel

BATCH, HEADS, SEQ, DIM = 2, 8, 512, 64
ITERATIONS = 5
SEED = 20261024
ATOL = 2e-3


def main() -> None:
    start = time.perf_counter()
    if not torch.cuda.is_available():
        raise SystemExit("bench: no CUDA device")
    device = torch.device("cuda")
    gen = torch.Generator(device="cpu").manual_seed(SEED)
    q, k, v = (
        torch.randn(BATCH, HEADS, SEQ, DIM, generator=gen, dtype=torch.float32).to(device)
        for _ in range(3)
    )
    scores = (q.double() @ k.double().transpose(-1, -2)) / DIM**0.5
    reference = torch.softmax(scores, dim=-1) @ v.double()

    times = []
    matched = True
    for _ in range(ITERATIONS):
        torch.cuda.synchronize()
        t = time.perf_counter()
        out = kernel.attention(q.clone(), k.clone(), v.clone())
        torch.cuda.synchronize()
        times.append((time.perf_counter() - t) * 1e3)
        matched &= out.shape == reference.shape and bool(
            torch.allclose(out.double(), reference, atol=ATOL, rtol=0)
        )

    rounded = torch.round(reference.cpu() * 1e3).to(torch.int64).numpy().tobytes()
    checksum = hashlib.sha256(rounded).hexdigest() if matched else "mismatch"
    p50 = statistics.median(times)
    rows = BATCH * HEADS * SEQ
    print(
        "optimize-loop-result "
        + json.dumps(
            {
                "variant": os.environ.get("BENCH_VARIANT", ""),
                "pod": socket.gethostname(),
                "records": rows,
                "iterations": ITERATIONS,
                "p50_ms": p50,
                "ops_per_s": rows / (p50 / 1e3),
                "total_ms": (time.perf_counter() - start) * 1e3,
                "checksum": checksum,
                "device": torch.cuda.get_device_name(device),
                "torch": torch.__version__,
            }
        ),
        flush=True,
    )


if __name__ == "__main__":
    main()
