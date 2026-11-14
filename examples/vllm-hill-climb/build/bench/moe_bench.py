"""The fused MoE benchmark: vLLM's routing and expert path at Qwen3-30B-A3B's shapes on one GPU.

For each token count it routes fixed inputs with `fused_topk`, runs `fused_experts`, and checks the
output against a float32 reference computed here from its own top-k. It then times both calls
replayed from a CUDA graph and prints one line:

    hill-climb-result {"ok": true, "geomean_us": ..., "us": {"1": ..., ...}, ...}

`ok` is false, with the token counts that missed, when any output is off by more than ATOL.
"""

import json
import math
import os
import statistics
import time

import torch

from vllm.model_executor.layers.fused_moe import fused_experts, fused_topk

EXPERTS, TOPK, HIDDEN, INTERMEDIATE = 128, 8, 2048, 768
TOKENS = [1, 8, 32, 128, 512, 2048, 8192]
CALLS_PER_GRAPH = 10
REPLAYS = 20
SEED = 20261109
ATOL = 2e-2


def reference(x: torch.Tensor, w1: torch.Tensor, w2: torch.Tensor, gating: torch.Tensor):
    probs = torch.softmax(gating.float(), dim=-1)
    weights, ids = torch.topk(probs, TOPK, dim=-1)
    weights = weights / weights.sum(dim=-1, keepdim=True)
    xf, w1f, w2f = x.float(), w1.float(), w2.float()
    out = torch.zeros(x.shape[0], HIDDEN, dtype=torch.float32, device=x.device)
    for e in range(EXPERTS):
        rows, slots = (ids == e).nonzero(as_tuple=True)
        if rows.numel() == 0:
            continue
        h = xf[rows] @ w1f[e].t()
        gate, up = h[:, :INTERMEDIATE], h[:, INTERMEDIATE:]
        y = (torch.nn.functional.silu(gate) * up) @ w2f[e].t()
        out.index_add_(0, rows, y * weights[rows, slots].unsqueeze(-1))
    return out


def moe(x: torch.Tensor, w1: torch.Tensor, w2: torch.Tensor, gating: torch.Tensor):
    weights, ids, _ = fused_topk(x, gating, TOPK, renormalize=True)
    return fused_experts(x, w1, w2, weights, ids)


def timed(x, w1, w2, gating) -> float:
    for _ in range(3):
        moe(x, w1, w2, gating)
    torch.cuda.synchronize()
    stream = torch.cuda.Stream()
    graph = torch.cuda.CUDAGraph()
    with torch.cuda.stream(stream), torch.cuda.graph(graph, stream=stream):
        for _ in range(CALLS_PER_GRAPH):
            moe(x, w1, w2, gating)
    torch.cuda.synchronize()
    graph.replay()
    torch.cuda.synchronize()
    start, end = torch.cuda.Event(enable_timing=True), torch.cuda.Event(enable_timing=True)
    samples = []
    for _ in range(REPLAYS):
        start.record()
        graph.replay()
        end.record()
        end.synchronize()
        samples.append(start.elapsed_time(end) * 1e3 / CALLS_PER_GRAPH)
    return statistics.median(samples)


def main() -> None:
    began = time.monotonic()
    if not torch.cuda.is_available():
        raise SystemExit("moe_bench: no CUDA device")
    torch.cuda.set_device(0)
    gen = torch.Generator(device="cpu").manual_seed(SEED)
    dtype, dev = torch.bfloat16, torch.device("cuda")

    def rand(*shape: int) -> torch.Tensor:
        return (torch.randn(*shape, generator=gen) / 10).to(dtype=dtype, device=dev)

    w1 = rand(EXPERTS, 2 * INTERMEDIATE, HIDDEN)
    w2 = rand(EXPERTS, HIDDEN, INTERMEDIATE)
    us: dict[str, float] = {}
    err: dict[str, float] = {}
    for m in TOKENS:
        x = rand(m, HIDDEN)
        gating = torch.randn(m, EXPERTS, generator=gen).to(dev)
        got = moe(x, w1, w2, gating).float()
        want = reference(x, w1, w2, gating)
        err[str(m)] = (got - want).abs().max().item() if got.shape == want.shape else math.inf
        us[str(m)] = round(timed(x, w1, w2, gating), 2)
    missed = [m for m, e in err.items() if not e <= ATOL]
    print(
        "hill-climb-result "
        + json.dumps(
            {
                "ok": not missed,
                "missed": missed,
                "geomean_us": round(math.exp(statistics.fmean(math.log(v) for v in us.values())), 3),
                "us": us,
                "max_abs_err": {m: round(e, 5) for m, e in err.items()},
                "gpu": torch.cuda.get_device_name(0),
                "label": os.environ.get("BENCH_LABEL", ""),
                "wall_s": round(time.monotonic() - began, 1),
            },
            sort_keys=True,
        ),
        flush=True,
    )


if __name__ == "__main__":
    main()
