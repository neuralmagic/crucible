params = {
    "vllm_repo": {
        "type": "string",
        "default": "https://github.com/vllm-project/vllm.git",
        "doc": "git URL to fetch vLLM from: upstream, a fork, anything the run can reach",
    },
    "vllm_ref": {
        "type": "string",
        "default": "5df63a7203cdb9328d824a27b682e77c7b9928be",
        "doc": "branch, tag, commit sha, or refs/pull/N/head",
    },
    "base_image": {
        "type": "string",
        "default": "docker.io/vllm/vllm-openai:v0.34.0@sha256:e40eb98263fa13418dda684147d73e8ec4a2b0e056a37e955dc189cd1b11dd16",
        "doc": "release image the ref compiles on; pick the release nearest the ref's fork point or the dependency sources will not compile against its torch",
    },
    "arch_list": {
        "type": "string",
        "default": "10.0",
        "doc": "TORCH_CUDA_ARCH_LIST for the compile; 10.0 is GB200",
    },
    "max_jobs": {
        "type": "int",
        "default": 64,
        "min": 1,
        "max": 96,
        "doc": "compile parallelism",
    },
    "build_timeout": {
        "type": "int",
        "default": 3600,
        "min": 300,
        "max": 3600,
        "doc": "buildit build timeout in seconds; the run's buildit policy caps this at 3600",
    },
    "builder": {
        "type": "string",
        "default": "buildah",
        "doc": "what builds the Dockerfile; buildkit needs user namespaces this cluster's apparmor forbids",
    },
}

build = command(
    name = "build",
    run = "python3 build.py",
    emits = ["image", "vllm_sha", "build_id", "seconds"],
    timeout = "210m",
)
workflow(type = "playbook", tasks = [build])
