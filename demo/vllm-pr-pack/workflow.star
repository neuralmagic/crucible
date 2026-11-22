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
        "default": "docker.io/vllm/vllm-openai:v0.32.0@sha256:befccf0e7e4089aa23926bb6f887a85c0d46c2a99e6de5c5ffdf5cb7957c5b34",
        "doc": "release image the ref compiles on; multi-arch, arm64 resolves on this cluster",
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
