"""E2E comparison on one GPU: a kernel modification's vLLM against the stock release image.

Builds the modification through the run's buildit into a runtime artifact, then serves the
same model twice in pods on the stock release image — once plain, once with the artifact
loaded in front of the entrypoint — and reports the throughput and latency difference. The
pods run sequentially on one GPU so both sides see the same hardware state, and the only
difference between them is the artifact."""

import hashlib
import json
import os
import ssl
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

SA = "/var/run/secrets/kubernetes.io/serviceaccount"


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


def build_artifact(params):
    src = Path("build/vllm")
    subprocess.run(["git", "init", "-q", str(src)], check=True)
    subprocess.run(
        ["git", "-C", str(src), "fetch", "-q", "--depth", "1",
         "https://github.com/vllm-project/vllm.git", params["vllm_ref"]],
        check=True,
    )
    subprocess.run(["git", "-C", str(src), "checkout", "-q", "--detach", "FETCH_HEAD"], check=True)
    sha = subprocess.run(
        ["git", "-C", str(src), "rev-parse", "HEAD"],
        capture_output=True, text=True, check=True,
    ).stdout.strip()
    print(f"vllm {params['vllm_ref']} at {sha}", flush=True)
    reqs = (src / "requirements" / "build" / "cuda.txt").read_bytes()
    (Path("build") / "reqlock.key").write_text(
        f"{params['base_image']}\n{hashlib.sha256(reqs).hexdigest()}\n"
    )
    reply = call(
        "build", "--no-wait",
        "-c", "build",
        "-f", "Dockerfile",
        "--builder", "buildah",
        "--ccache",
        "--cache", "vllm-ccache=/root/.cache/ccache",
        "--cache", "vllm-deps=/src/vllm/.deps:cluster",
        "--cache-key", "vllm-deps=vllm/CMakeLists.txt",
        "--cache-key", "vllm-deps=vllm/cmake/external_projects",
        "--cache", "vllm-reqlock=/reqlock:cluster",
        "--cache-key", "vllm-reqlock=reqlock.key",
        "--build-arg", f"BASE={params['base_image']}",
        "--build-arg", "CUDA_ARCH_LIST=10.0",
        "--build-arg", f"MAX_JOBS={params.get('max_jobs', 24)}",
        "--build-arg", f"ARTIFACT={sha}.tar",
        "--build-arg", f"REGISTRY={os.environ['MCP_IMAGE_REPO'].partition('/')[0]}",
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
    print(f"build state: {state}", flush=True)
    if state != "succeeded":
        tail = call("logs", build_id, "--tail", "150")
        print(json.dumps({"status": status, "logs": tail})[-6000:])
        sys.exit(f"build {state}")
    facts = status.get("facts") or {}
    digest = facts.get("artifact_digest")
    if not digest:
        sys.exit("the build left no artifact digest in its facts")
    registry = os.environ["MCP_IMAGE_REPO"].partition("/")[0]
    return f"{registry}/v2/artifacts/blobs/{digest}"


def k8s(method, path, body=None):
    host, port = os.environ["KUBERNETES_SERVICE_HOST"], os.environ["KUBERNETES_SERVICE_PORT"]
    token = Path(f"{SA}/token").read_text()
    ctx = ssl.create_default_context(cafile=f"{SA}/ca.crt")
    req = urllib.request.Request(
        f"https://{host}:{port}{path}",
        method=method,
        data=json.dumps(body).encode() if body is not None else None,
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
            "Accept": "application/json",
        },
    )
    with urllib.request.urlopen(req, context=ctx, timeout=120) as r:
        text = r.read().decode()
    return json.loads(text) if text.strip() else None


def k8s_logs(name, namespace):
    host, port = os.environ["KUBERNETES_SERVICE_HOST"], os.environ["KUBERNETES_SERVICE_PORT"]
    token = Path(f"{SA}/token").read_text()
    ctx = ssl.create_default_context(cafile=f"{SA}/ca.crt")
    req = urllib.request.Request(
        f"https://{host}:{port}/api/v1/namespaces/{namespace}/pods/{name}/log?container=vllm",
        headers={"Authorization": f"Bearer {token}"},
    )
    with urllib.request.urlopen(req, context=ctx, timeout=120) as r:
        return r.read().decode()


BENCH = r"""
if [ -d /artifact/vllm ]; then export PYTHONPATH=/artifact; fi
export HF_HOME=/tmp/hf
vllm serve "$MODEL" --served-model-name m --max-model-len 2048 --gpu-memory-utilization 0.85 --port 8000 >/tmp/serve.log 2>&1 &
for i in $(seq 240); do curl -sf http://127.0.0.1:8000/health >/dev/null 2>&1 && break; sleep 5; done
curl -sf http://127.0.0.1:8000/health >/dev/null || { echo SERVE_FAILED; tail -40 /tmp/serve.log; exit 1; }
python3 - <<'PY'
import json, os, time, urllib.request
reqs = int(os.environ["REQUESTS"])
max_tokens = int(os.environ["MAX_TOKENS"])
body = json.dumps({"model": "m", "prompt": "Write a short story about a robot.", "max_tokens": max_tokens}).encode()
latencies, tokens = [], 0
started = time.monotonic()
for _ in range(reqs):
    s = time.monotonic()
    r = json.load(urllib.request.urlopen(urllib.request.Request(
        "http://127.0.0.1:8000/v1/completions", data=body,
        headers={"Content-Type": "application/json"}), timeout=600))
    latencies.append(time.monotonic() - s)
    tokens += r["usage"]["completion_tokens"]
wall = time.monotonic() - started
print("BENCH_RESULT " + json.dumps({
    "requests": reqs,
    "output_tokens": tokens,
    "wall_s": round(wall, 2),
    "output_tps": round(tokens / wall, 2),
    "mean_latency_s": round(sum(latencies) / len(latencies), 3),
}))
PY
"""


def run_bench(name, artifact_url, params, namespace):
    volumes = [{"name": "hf", "emptyDir": {}}]
    mounts = [{"name": "hf", "mountPath": "/tmp/hf"}]
    init = []
    if artifact_url:
        volumes.append({"name": "artifact", "emptyDir": {}})
        mounts.append({"name": "artifact", "mountPath": "/artifact"})
        init.append({
            "name": "fetch",
            "image": "quay.io/buildah/stable:latest",
            "command": ["/bin/bash", "-ceu"],
            "args": [f"curl -sf '{artifact_url}' | tar -xf - -C /artifact"],
            "volumeMounts": [{"name": "artifact", "mountPath": "/artifact"}],
        })
    pod = {
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name, "namespace": namespace, "labels": {"app": "vllm-bench"}},
        "spec": {
            "restartPolicy": "Never",
            "activeDeadlineSeconds": 2400,
            "nodeSelector": {
                "kubernetes.io/arch": "arm64",
                "nvidia.com/gpu.product": "NVIDIA-GB200",
            },
            "tolerations": [
                {"key": "nvidia.com/gpu", "operator": "Exists", "effect": "NoSchedule"},
            ],
            "initContainers": init,
            "containers": [{
                "name": "vllm",
                "image": params["base_image"],
                "command": ["/bin/bash", "-ceu"],
                "args": [BENCH],
                "env": [
                    {"name": "MODEL", "value": params["model"]},
                    {"name": "REQUESTS", "value": str(params["requests"])},
                    {"name": "MAX_TOKENS", "value": str(params["max_tokens"])},
                ],
                "resources": {
                    "requests": {"nvidia.com/gpu": 1, "cpu": "8", "memory": "32Gi"},
                    "limits": {"nvidia.com/gpu": 1, "cpu": "16", "memory": "64Gi"},
                },
                "volumeMounts": mounts,
            }],
            "volumes": volumes,
        },
    }
    print(f"scheduling {name} on one gpu", flush=True)
    k8s("POST", f"/api/v1/namespaces/{namespace}/pods", pod)
    deadline = time.monotonic() + 2700
    phase = "Pending"
    while time.monotonic() < deadline:
        phase = k8s("GET", f"/api/v1/namespaces/{namespace}/pods/{name}")["status"]["phase"]
        if phase in ("Succeeded", "Failed"):
            break
        time.sleep(20)
    logs = k8s_logs(name, namespace)
    k8s("DELETE", f"/api/v1/namespaces/{namespace}/pods/{name}")
    if phase != "Succeeded":
        sys.exit(f"{name} ended {phase}:\n{logs[-3000:]}")
    for line in logs.splitlines():
        if line.startswith("BENCH_RESULT "):
            return json.loads(line[len("BENCH_RESULT "):])
    sys.exit(f"{name} produced no bench result:\n{logs[-3000:]}")


def main():
    params = json.loads(Path(os.environ["CRUCIBLE_INPUTS_FILE"]).read_text())["params"]
    namespace = Path(f"{SA}/namespace").read_text().strip()
    artifact = params.get("artifact", "").strip() or build_artifact(params)
    print(f"artifact {artifact}", flush=True)
    tag = artifact.rsplit(":", 1)[-1][:8]
    baseline = run_bench(f"vllm-bench-baseline-{tag}", None, params, namespace)
    print(f"baseline {json.dumps(baseline)}", flush=True)
    candidate = run_bench(f"vllm-bench-candidate-{tag}", artifact, params, namespace)
    print(f"candidate {json.dumps(candidate)}", flush=True)
    delta = (candidate["output_tps"] - baseline["output_tps"]) / baseline["output_tps"] * 100
    print(json.dumps({
        "baseline": baseline,
        "candidate": candidate,
        "delta_output_tps_pct": round(delta, 1),
    }))


if __name__ == "__main__":
    main()
