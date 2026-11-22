"""Check a vLLM ref out of any repo and build it on a release image through the run's buildit."""

import json
import os
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

    src = Path("build/vllm")
    subprocess.run(["git", "init", "-q", str(src)], check=True)
    subprocess.run(
        ["git", "-C", str(src), "fetch", "-q", "--depth", "1",
         params["vllm_repo"], params["vllm_ref"]],
        check=True,
    )
    subprocess.run(["git", "-C", str(src), "checkout", "-q", "--detach", "FETCH_HEAD"], check=True)
    sha = subprocess.run(
        ["git", "-C", str(src), "rev-parse", "HEAD"],
        capture_output=True, text=True, check=True,
    ).stdout.strip()
    print(f"vllm {params['vllm_ref']} at {sha}", flush=True)

    started = time.monotonic()
    reply = call(
        "build", "--no-wait",
        "-c", "build",
        "-f", "Dockerfile",
        "--builder", params["builder"],
        "--ccache",
        "--cache", "vllm-ccache=/root/.cache/ccache",
        "--cache", "vllm-deps=/src/vllm/.deps:cluster",
        "--cache-key", "vllm-deps=vllm/CMakeLists.txt",
        "--cache-key", "vllm-deps=vllm/cmake/external_projects",
        "--build-arg", f"BASE={params['base_image']}",
        "--build-arg", f"CUDA_ARCH_LIST={params['arch_list']}",
        "--build-arg", f"MAX_JOBS={params['max_jobs']}",
        "--timeout", str(params["build_timeout"]),
    )
    build_id = reply["build_id"]
    print(f"submitted {build_id}", flush=True)

    while True:
        status = call("status", build_id)
        state = status.get("status")
        print(f"state: {state}", flush=True)
        if state != "running":
            break
        time.sleep(5)

    if state != "succeeded":
        tail = call("logs", build_id, "--tail", "150")
        print(json.dumps({"status": status, "logs": tail})[-6000:])
        sys.exit(f"build {state}")

    print(json.dumps({
        "image": image_ref(build_id),
        "vllm_sha": sha,
        "build_id": build_id,
        "seconds": round(time.monotonic() - started),
    }))


if __name__ == "__main__":
    main()
