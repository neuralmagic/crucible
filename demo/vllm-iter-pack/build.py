"""Iterate on vLLM CUDA kernels: diff a ref against the base image's commit, apply the diff on
top of the compiled tree inside the image, recompile only what changed, and optionally bench."""

import json
import os
import shutil
import subprocess
import sys
import time
import urllib.request
from pathlib import Path


def call(*args, timeout=180):
    try:
        p = subprocess.run(["buildit", *args], capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        sys.exit(f"buildit {args[0]} did not answer within {timeout}s")
    lines = p.stdout.strip().splitlines()
    reply = {}
    if lines and lines[-1].lstrip().startswith("{"):
        try:
            reply = json.loads(lines[-1])
        except json.JSONDecodeError:
            pass
    if p.returncode != 0:
        sys.exit(
            f"buildit {args[0]} exited {p.returncode}: "
            f"{json.dumps(reply)[-2500:]} {p.stderr[-2500:]}"
        )
    return reply


def image_ref(build_id):
    repo = os.environ["MCP_IMAGE_REPO"]
    host, _, name = repo.partition("/")
    for _ in range(6):
        with urllib.request.urlopen(f"http://{host}/v2/{name}/tags/list", timeout=30) as r:
            tags = json.load(r)["tags"]
        match = [t for t in tags if t.endswith(f"-buildit-{build_id}")]
        if match:
            return f"{repo}:{match[0]}"
        time.sleep(5)
    return f"{repo}:{build_id}"


def main():
    params = json.loads(Path(os.environ["CRUCIBLE_INPUTS_FILE"]).read_text())["params"]

    src = Path("iter/vllm")
    subprocess.run(["git", "init", "-q", str(src)], check=True)
    subprocess.run(
        ["git", "-C", str(src), "fetch", "-q", "--depth", "1",
         params["vllm_repo"], params["vllm_ref"]],
        check=True,
    )
    subprocess.run(["git", "-C", str(src), "checkout", "-q", "--detach", "FETCH_HEAD"], check=True)
    head = subprocess.run(
        ["git", "-C", str(src), "rev-parse", "HEAD"],
        capture_output=True, text=True, check=True,
    ).stdout.strip()
    subprocess.run(
        ["git", "-C", str(src), "fetch", "-q", "--depth", "1",
         params["vllm_repo"], params["base_sha"]],
        check=True,
    )
    diff = subprocess.run(
        ["git", "-C", str(src), "diff", params["base_sha"], head,
         "--", ".",
         ":(exclude)vllm/benchmarks", ":(exclude)tests", ":(exclude)docs",
         ":(exclude)examples", ":(exclude).github"],
        capture_output=True, text=True, check=True,
    ).stdout
    files = [l[6:] for l in diff.splitlines() if l.startswith("diff --git ")]
    print(f"{params['vllm_ref']} at {head}: {len(files)} files changed vs {params['base_sha'][:12]}", flush=True)
    if not diff:
        sys.exit("the ref matches the base image's commit; nothing to iterate")

    bench = params["bench_cmd"].strip()
    ctx = Path("context")
    ctx.mkdir(exist_ok=True)
    (ctx / "kernel.patch").write_text(diff)

    if params["mode"] == "image":
        context = "iter/vllm"
        script = "cd /src/vllm && python3 setup.py build_ext --inplace"
        if bench:
            script += f" && ( {bench} )"
        build_args = [
            "--base", params["base_image"],
            "--script", script,
            "--path", "/src/vllm",
            "--ccache",
            "--cache", "vllm-ccache=/root/.cache/ccache",
            "--cache", "vllm-deps=/src/vllm/.deps:cluster",
            "--cache-key", "vllm-deps=vllm/CMakeLists.txt",
            "--cache-key", "vllm-deps=vllm/cmake/external_projects",
        ]
    elif params["mode"] == "layer":
        context = "context"
        script = "cd /src/vllm && git apply /iter/kernel.patch && python3 setup.py build_ext --inplace"
        if bench:
            script += f" && ( {bench} )"
        build_args = ["--base", params["base_image"], "--script", script, "--path", "/iter"]
    else:
        context = "context"
        for rel in ("CMakeLists.txt", "cmake/external_projects"):
            key_src = src / rel
            key_dst = ctx / "vllm" / rel
            key_dst.parent.mkdir(parents=True, exist_ok=True)
            if key_src.is_dir():
                shutil.copytree(key_src, key_dst, dirs_exist_ok=True)
            else:
                shutil.copy2(key_src, key_dst)
        run = "cd /src/vllm && git apply /tmp/kernel.patch"
        run += ' && TORCH_CUDA_ARCH_LIST="$CUDA_ARCH_LIST" MAX_JOBS="$MAX_JOBS" python3 setup.py build_ext --inplace'
        if bench:
            run += f" && ( {bench} )"
        (ctx / "Dockerfile").write_text(
            "ARG BASE\n"
            "FROM ${BASE}\n"
            "COPY kernel.patch /tmp/kernel.patch\n"
            "ARG CUDA_ARCH_LIST=10.0\n"
            "ARG MAX_JOBS=64\n"
            f"RUN {run}\n"
            "ARG ARTIFACT=\"\"\n"
            "ARG REGISTRY=\"\"\n"
            "RUN if [ -n \"$ARTIFACT\" ]; then \\\n"
            "        cd /src/vllm && \\\n"
            "        tar -cf /tmp/artifact.tar --exclude=.deps --exclude=build --exclude=rust/target . && \\\n"
            "        d=sha256:$(sha256sum /tmp/artifact.tar | cut -d\" \" -f1) && \\\n"
            "        loc=$(curl -sf -X POST \"$REGISTRY/v2/artifacts/blobs/uploads/\" -o /dev/null -w \"%header{location}\") && \\\n"
            "        case \"$loc\" in http*) : ;; /*) loc=\"$REGISTRY$loc\" ;; *) loc=\"$REGISTRY/$loc\" ;; esac && \\\n"
            "        sep=? && case \"$loc\" in *\\?*) sep=\"&\" ;; esac && \\\n"
            "        curl -sf -X PUT -H \"Content-Type: application/octet-stream\" -T /tmp/artifact.tar \"$loc${sep}digest=$d\" && \\\n"
            "        printf '{\"artifact_digest\": \"%s\", \"artifact_bytes\": %s}\\n' \"$d\" \"$(wc -c < /tmp/artifact.tar)\" > /buildit/facts.json && \\\n"
            "        echo \"artifact $d at $REGISTRY/v2/artifacts\" && \\\n"
            "        rm -f /tmp/artifact.tar; \\\n"
            "    fi\n"
        )
        build_args = [
            "-f", "Dockerfile",
            "--build-arg", f"BASE={params['base_image']}",
            "--build-arg", f"CUDA_ARCH_LIST={params['arch_list']}",
            "--build-arg", f"MAX_JOBS={params['max_jobs']}",
            "--build-arg", f"ARTIFACT={head}.tar",
            "--build-arg", f"REGISTRY={os.environ['MCP_IMAGE_REPO'].partition('/')[0]}",
            "--builder", "buildah",
            "--ccache",
            "--cache", "vllm-ccache=/root/.cache/ccache",
            "--cache", "vllm-deps=/src/vllm/.deps:cluster",
            "--cache-key", "vllm-deps=vllm/CMakeLists.txt",
            "--cache-key", "vllm-deps=vllm/cmake/external_projects",
        ]

    started = time.monotonic()
    reply = call(
        "build", "--no-wait",
        "-c", context,
        *build_args,
        "--timeout", str(params["build_timeout"]),
    )
    build_id = reply["build_id"]
    print(f"submitted {build_id}", flush=True)

    while True:
        status = call("status", build_id, timeout=60)
        state = status.get("status")
        if state != "running":
            break
        time.sleep(5)
    print(f"state: {state}", flush=True)
    if state != "succeeded":
        tail = call("logs", build_id, "--tail", "150")
        print(json.dumps({"status": status, "logs": tail})[-6000:])
        sys.exit(f"build {state}")

    facts = status.get("facts") or {}
    registry = os.environ["MCP_IMAGE_REPO"].partition("/")[0]
    print(json.dumps({
        "image": image_ref(build_id),
        "artifact": f"{registry}/v2/artifacts/blobs/{facts.get('artifact_digest', '?')}",
        "seconds": round(time.monotonic() - started),
        "patch_files": files,
        "bench": status.get("facts") or None,
    }))


if __name__ == "__main__":
    main()
