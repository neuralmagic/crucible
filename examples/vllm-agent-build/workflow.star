params = {
    "base_image": {"type": "string", "default": "docker.io/vllm/vllm-openai:v0.32.0@sha256:befccf0e7e4089aa23926bb6f887a85c0d46c2a99e6de5c5ffdf5cb7957c5b34"},
    "vllm_ref": {"type": "string", "default": "5df63a7203cdb9328d824a27b682e77c7b9928be"},
    "arch_list": {"type": "string", "default": "10.0"},
    "max_jobs": {"type": "int", "default": 64, "min": 1, "max": 128},
}

BUILDS = ["checkout", "edit1", "edit2", "edit3"]
LOGS = ["traces/%s.%s" % (b, log) for b in BUILDS for log in ["ninja_log", "ccache-stats.log"]]

build = agent(
    name = "build",
    prompt = prompt_file("prompt.md") +
             "\n\nvllm_ref: " + param("vllm_ref") +
             "\nBASE: " + param("base_image") +
             "\nTORCH_CUDA_ARCH_LIST: " + param("arch_list") +
             "\nMAX_JOBS: " + str(param("max_jobs")) + "\n",
    emits_files = ["report.json"] + LOGS,
    timeout = "120m",
)

trace = command(
    name = "trace",
    run = "python3 trace/render.py inputs/build",
    depends_on = [build],
    emits_files = ["traces/%s.html" % b for b in BUILDS] + ["traces/index.html"],
)

workflow(type = "playbook", tasks = [build, trace], result = trace)
