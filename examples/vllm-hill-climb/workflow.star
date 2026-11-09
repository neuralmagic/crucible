params = {
    "base_image": {"type": "string", "default": "docker.io/vllm/vllm-openai:v0.32.0@sha256:befccf0e7e4089aa23926bb6f887a85c0d46c2a99e6de5c5ffdf5cb7957c5b34"},
    "vllm_ref": {"type": "string", "default": "5df63a7203cdb9328d824a27b682e77c7b9928be"},
    "arch_list": {"type": "string", "default": "8.0", "doc": "TORCH_CUDA_ARCH_LIST; 8.0 is the A100"},
    "max_jobs": {"type": "int", "default": 64, "min": 1, "max": 128},
    "serve_model": {"type": "string", "default": "Qwen/Qwen3-30B-A3B", "doc": "config and tokenizer the serving check runs with dummy weights"},
    "queue": {"type": "string", "default": "crucible-e2e", "doc": "the LocalQueue the GPU Jobs go to"},
    "rounds": {"type": "int", "default": 3, "min": 1, "max": 6, "doc": "climb rounds after the baseline"},
}

seed = command(
    name = "seed",
    run = "python3 seed.py",
    emits = ["seed_image", "seed_build_id", "seed_build_seconds", "seed_push_seconds", "seed_ccache"],
    timeout = "60m",
)

climb = agent(
    name = "climb",
    prompt = prompt_file("prompt.md") +
             "\n\nvllm_ref: " + param("vllm_ref") +
             "\nBASE: " + param("base_image") +
             "\nCUDA_ARCH_LIST: " + param("arch_list") +
             "\nMAX_JOBS: " + str(param("max_jobs")) +
             "\nSERVE_MODEL: " + param("serve_model") +
             "\nQUEUE: " + param("queue") +
             "\nN: " + str(param("rounds")) + "\n",
    depends_on = [seed],
    emits_files = ["report.json", "timeline.jsonl", "results.jsonl", "best.patch"],
    timeout = "150m",
)

report = command(
    name = "report",
    run = "python3 report.py inputs/climb",
    depends_on = [seed, climb],
    emits = ["speedup", "best", "builds", "layer_builds", "median_iteration_s", "median_queue_run_s"],
    emits_files = ["summary.md"],
)

workflow(type = "playbook", tasks = [seed, climb, report], result = report)
