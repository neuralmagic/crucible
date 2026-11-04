params = {
    "base_image": {
        "type": "string",
        "default": "docker.io/vllm/vllm-openai:v0.32.0@sha256:befccf0e7e4089aa23926bb6f887a85c0d46c2a99e6de5c5ffdf5cb7957c5b34",
        "doc": "a public vLLM image built from vllm_ref",
    },
    "vllm_ref": {
        "type": "string",
        "default": "5df63a7203cdb9328d824a27b682e77c7b9928be",
        "doc": "the vLLM commit base_image was built from",
    },
    "arch_list": {"type": "string", "default": "10.0", "doc": "TORCH_CUDA_ARCH_LIST of the builds"},
    "max_jobs": {"type": "int", "default": 16, "min": 1, "max": 256, "doc": "MAX_JOBS of the builds"},
    "push": {"type": "bool", "default": True, "doc": "push each image and time it"},
    "timeout_s": {"type": "int", "default": 3600, "min": 600, "max": 3600, "doc": "each build's timeout"},
}

STEPS = ["clean", "leaf", "leaf_py", "revert", "leaf_again", "header", "cmake"]

def chain(names):
    tasks = []
    for name in names:
        tasks.append(command(
            name = name,
            run = "python3 bench/replay.py step " + name,
            depends_on = tasks[-1:],
            timeout = "90m",
        ))
    return tasks

steps = chain(STEPS)

summary = evaluate(
    name = "report",
    run = "python3 bench/replay.py report",
    depends_on = steps,
    emits = {"steps": "list", "candidate_s": "number"},
)

workflow(type = "playbook", tasks = steps + [summary], result = summary)
