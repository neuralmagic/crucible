# vllm-hill-climb

An agent hill climbs vLLM's fused MoE path on an A100: it proposes an edit, builds it through the
run's `buildit`, measures it as a Kueue Job, keeps it only if it is faster, and repeats.

```text
seed (command: vLLM at vllm_ref built once, pushed; ccache counted)
  -> climb (agent: baseline, then rounds of 2-3 candidates; keeps the fastest correct one)
     -> report (command: speedup, per-step times, build reuse -> summary.md)
```

| param | default | meaning |
| --- | --- | --- |
| `base_image` | vllm-openai v0.32.0 | the release image the seed builds on |
| `vllm_ref` | `5df63a72` | the vLLM commit every candidate starts from |
| `arch_list` | `8.0` | `TORCH_CUDA_ARCH_LIST`; 8.0 is the A100 |
| `serve_model` | `Qwen/Qwen3-30B-A3B` | config and tokenizer for the serving check (dummy weights) |
| `queue` | `crucible-e2e` | the LocalQueue the GPU Jobs go to |
| `rounds` | `3` | climb rounds after the baseline |

## What is measured

`build/bench/moe_bench.py` runs `fused_topk` and `fused_experts` at Qwen3-30B-A3B's shapes (128
experts, top 8, hidden 2048, intermediate 768, bf16) for 1 to 8192 tokens, checks each output
against a float32 reference, times both replayed from a CUDA graph, and prints one
`hill-climb-result` line. The score is the geometric mean over token counts. After the climb the
agent runs `build/bench/serve_bench.sh` on the seed and the kept candidate: `vllm serve` with
dummy weights, then `vllm bench serve`.

## How candidates build

`hc.py stage` diffs the checkout against the seed. Edits that are all Python or JSON under
`vllm/` build as a buildit layer over the seed image: the edited files appended as one layer,
nothing compiled, the seed's layers neither pulled nor pushed. Anything else (csrc, CMake) builds
from the Dockerfile with the run's ccache and cluster `.deps` caches. `hc.py keep` makes a
candidate the tree the next round starts from; `best.patch` is the kept diff.

## Run it

Launch the draft from Crucible Playbooks on a cluster the shared buildit cache serves (`pirate`),
with an agent provider and model, under a time ceiling of 3h. The A100 Jobs need a queue
whose flavors offer `NVIDIA-A100-SXM4-80GB`.

```sh
cargo test -p crucible --test vllm_hill_climb_example
```
