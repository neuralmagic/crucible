# vLLM fused MoE hill climb: 1.0745x

Kept `kern1`: 633.49 us geomean against the seed's 680.657 us.

| round | label | recipe | ok | geomean us | speedup | build+push s | queue+run s | iteration s |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 0 | baseline | seed | yes | 680.657 | 1.0 | None | None | 2652.1 |
| 1 | cfgsmall-layer | layer | no | None |  | None | None | None |
| 1 | cfgwide-layer | layer | no | None |  | None | None | None |
| 1 | cfgsmall-fromseed | dockerfile | no | None |  | None | None | None |
| 1 | cfgsmall | dockerfile | yes | 665.825 | 1.0223 | 589.2 | 517.5 | 1118.4 |
| 1 | cfgwide | dockerfile | yes | 659.182 | 1.0326 | 915.6 | 475.7 | 1399.1 |
| 1 | cfgh200 | dockerfile | yes | 645.672 | 1.0542 | 1173.2 | 195.6 | 1376.8 |
| 2 | merge1 | dockerfile | yes | 646.89 | 1.0522 | 441.8 | 479.2 | 932.5 |
| 2 | cfgmid | dockerfile | yes | 652.031 | 1.0439 | 687.0 | 183.8 | 949.2 |
| 2 | kern1 | dockerfile | yes | 633.49 | 1.0745 | 844.5 | None | 1096.0 |

Builds: 9, of which 2 were layers that compiled nothing.
Median iteration: {'layer': None, 'dockerfile': 1107.2}; build and push: {'layer': None, 'dockerfile': 765.8}; queue and run: 475.7 s.
Seed: 544 s to build, 174 s to push, ccache {'hit': 0, 'miss': 54, 'remote_hit': 304, 'uncached': 0}.

A per-token-count Triton config table tuned for the A100-SXM4-80GB plus mask-elision fast paths in fused_moe_kernel cut the fused MoE geomean from 680.657 us to 633.49 us (1.074x, 7.4% faster), with every token count improving and all outputs still within 2e-2 of the fp32 reference. Six buildable candidates were measured over three rounds; kern1 was the only one to clear the 1% bar over the best so far. The serving check could not run: both the seed and kern1 images fail identically at `vllm bench serve` startup with ModuleNotFoundError: No module named 'vllm.benchmarks.lib' (the vLLM repo's own .dockerignore excludes vllm/benchmarks/lib/ from the image), so no serving numbers were produced for either side.
