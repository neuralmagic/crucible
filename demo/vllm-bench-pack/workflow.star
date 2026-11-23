params = {
    "artifact": {
        "type": "string",
        "default": "",
        "doc": "an artifact blob URL from a vllm-pr-build or vllm-iter run whose tree matches base_image; empty builds vllm_ref",
    },
    "vllm_ref": {
        "type": "string",
        "default": "2f459ea8c1355bcb8428dc7544d87700c2cb3fde",
        "doc": "the kernel modification to compare: branch, tag, sha, or refs/pull/N/head",
    },
    "base_image": {
        "type": "string",
        "default": "docker.io/vllm/vllm-openai:v0.34.0@sha256:e40eb98263fa13418dda684147d73e8ec4a2b0e056a37e955dc189cd1b11dd16",
        "doc": "the release image both sides run on; the modification must compile against its torch, and the nodes must be able to pull it",
    },
    "model": {
        "type": "string",
        "default": "Qwen/Qwen2.5-0.5B-Instruct",
        "doc": "the model both sides serve; small keeps the download and boot quick",
    },
    "requests": {
        "type": "int",
        "default": 24,
        "min": 1,
        "max": 1000,
        "doc": "completion requests each side serves",
    },
    "max_tokens": {
        "type": "int",
        "default": 128,
        "min": 1,
        "max": 4096,
        "doc": "completion tokens per request",
    },
    "build_timeout": {
        "type": "int",
        "default": 3600,
        "min": 300,
        "max": 3600,
        "doc": "buildit build timeout in seconds",
    },
}

compare = command(
    name = "compare",
    run = "python3 build.py",
    emits = ["baseline", "candidate", "delta_output_tps_pct"],
    timeout = "210m",
)
workflow(type = "playbook", tasks = [compare])
