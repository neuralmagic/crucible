params = {
    "base_image": {"type": "string", "default": "docker.io/vllm/vllm-openai:v0.32.0@sha256:befccf0e7e4089aa23926bb6f887a85c0d46c2a99e6de5c5ffdf5cb7957c5b34"},
    "vllm_ref": {"type": "string", "default": "5df63a7203cdb9328d824a27b682e77c7b9928be"},
    "arch_list": {"type": "string", "default": "10.0"},
    "max_jobs": {"type": "int", "default": 64, "min": 1, "max": 128},
    "kernels": {
        "type": "string",
        "default": "layernorm,activation,pos_encoding,cache",
        "doc": "comma-separated csrc/<name>_kernels.cu files, one agent each",
    },
}

LOGS = ["ninja_log", "ccache-stats.log", "steps.log"]

seed = command(
    name = "seed",
    run = "python3 seed.py",
    emits = ["kernels", "seconds"],
    emits_files = ["traces/seed.%s" % log for log in LOGS],
    timeout = "90m",
)

edit = agent(
    name = "edit",
    prompt = prompt_file("prompt.md") +
             "\n\nvllm_ref: " + param("vllm_ref") +
             "\nBASE: " + param("base_image") +
             "\nCUDA_ARCH_LIST: " + param("arch_list") +
             "\nMAX_JOBS: " + str(param("max_jobs")) + "\n",
    depends_on = [seed],
    over = seed.kernels,
    max_fanout = 8,
    isolated = True,
    required = False,
    emits_files = ["report.json"] + ["traces/%s.%s" % (b, log) for b in ["checkout", "edit"] for log in LOGS],
    timeout = "120m",
)

trace = command(
    name = "trace",
    run = "python3 trace/render.py inputs",
    depends_on = [seed, edit],
    join = "passed",
    emits = ["builds"],
    emits_files = ["traces/index.html"],
)

workflow(type = "playbook", tasks = [seed, edit, trace], result = trace)
