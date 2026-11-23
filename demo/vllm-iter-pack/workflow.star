params = {
    "base_image": {
        "type": "string",
        "default": "registry.example.com/vllm/build-base:latest",
        "doc": "image carrying the compiled vllm tree to iterate on; the nodes must be able to pull it",
    },
    "vllm_repo": {
        "type": "string",
        "default": "https://github.com/vllm-project/vllm.git",
        "doc": "git URL the iterating ref lives in: upstream or a fork",
    },
    "vllm_ref": {
        "type": "string",
        "default": "refs/pull/69057/head",
        "doc": "branch, tag, sha, or refs/pull/N/head carrying the kernel edits",
    },
    "base_sha": {
        "type": "string",
        "default": "2f459ea8c1355bcb8428dc7544d87700c2cb3fde",
        "doc": "the commit the base image was built from; the iteration is diffed against it",
    },
    "bench_cmd": {
        "type": "string",
        "default": "",
        "doc": "shell command run inside the build after compiling; write JSON to /buildit/facts.json to emit it",
    },
    "mode": {
        "type": "string",
        "default": "build",
        "doc": "build: FROM the base over the pod network, works with any registry the builder pods reach; layer: pod runs the base directly, needs a node-pullable base and is the fastest; image: full-tree context as one runnable layer",
    },
    "arch_list": {
        "type": "string",
        "default": "10.0",
        "doc": "TORCH_CUDA_ARCH_LIST for the recompile; must match the base image's",
    },
    "max_jobs": {
        "type": "int",
        "default": 24,
        "min": 1,
        "max": 96,
        "doc": "compile parallelism",
    },
    "build_timeout": {
        "type": "int",
        "default": 3600,
        "min": 300,
        "max": 3600,
        "doc": "buildit build timeout in seconds",
    },
}

build = command(
    name = "build",
    run = "python3 build.py",
    emits = ["image", "seconds", "patch_files", "bench"],
    timeout = "210m",
)
workflow(type = "playbook", tasks = [build])
