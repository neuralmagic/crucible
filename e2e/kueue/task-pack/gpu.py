"""Run a GPU Job on each queue and serve a LeaderWorkerSet on a GPU, then report what ran."""

import asyncio
import json
import urllib.error
import urllib.request

from crucible_kueue import AsyncClient

QUEUES = {"a100": "crucible-e2e", "b200": "crucible-e2e-b200"}
GPU = {"requests": {"cpu": "1", "memory": "2Gi"}, "limits": {"nvidia.com/gpu": "1"}}
NAME_GPU = "nvidia-smi --query-gpu=name --format=csv,noheader"


def gpu_job(name: str) -> dict:
    return {
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {"name": name},
        "spec": {
            "backoffLimit": 0,
            "template": {
                "spec": {
                    "restartPolicy": "Never",
                    "containers": [
                        {
                            "name": "gpu",
                            "image": "@IMAGE@",
                            "command": ["sh", "-c", f"echo GPU $({NAME_GPU}) @NONCE@"],
                            "resources": GPU,
                        }
                    ],
                }
            },
        },
    }


SERVER = {
    "apiVersion": "leaderworkerset.x-k8s.io/v1",
    "kind": "LeaderWorkerSet",
    "metadata": {"name": "task-serve-@NONCE_SHORT@"},
    "spec": {
        "replicas": 1,
        "leaderWorkerTemplate": {
            "size": 1,
            "workerTemplate": {
                "spec": {
                    "containers": [
                        {
                            "name": "server",
                            "image": "@IMAGE@",
                            "command": [
                                "sh",
                                "-c",
                                f"mkdir /tmp/www && cd /tmp/www && {NAME_GPU} > gpu"
                                " && exec python3 -m http.server 8000",
                            ],
                            "ports": [{"containerPort": 8000}],
                            "readinessProbe": {"httpGet": {"path": "/gpu", "port": 8000}},
                            "resources": GPU,
                        }
                    ]
                }
            },
        },
    },
}


def client_job(url: str) -> dict:
    fetch = f"import urllib.request; print('SERVED', urllib.request.urlopen('{url}', timeout=10).read().decode())"
    return {
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {"name": "task-client-@NONCE_SHORT@"},
        "spec": {
            "backoffLimit": 4,
            "template": {
                "spec": {
                    "restartPolicy": "Never",
                    "containers": [
                        {
                            "name": "client",
                            "image": "@IMAGE@",
                            "command": ["python3", "-c", fetch],
                            "resources": {"requests": {"cpu": "100m", "memory": "64Mi"}},
                        }
                    ],
                }
            },
        },
    }


async def fetch(url: str) -> str:
    last = ""
    for _ in range(60):
        try:
            with urllib.request.urlopen(url, timeout=5) as response:
                return response.read().decode().strip()
        except (urllib.error.URLError, OSError) as error:
            last = str(error)
            await asyncio.sleep(1)
    raise RuntimeError(f"{url} never answered: {last}")


async def served(kueue: AsyncClient) -> dict:
    async with kueue.serving(QUEUES["a100"], SERVER, 8000, timeout=1200) as url:
        from_task = await fetch(f"{url}/gpu")
        from_job = await kueue.run(
            QUEUES["a100"], client_job(f"{url}/gpu"), timeout=600, grep="SERVED"
        )
    return {"url": url, "from_task": from_task, "from_job": from_job.strip()}


async def main() -> None:
    kueue = AsyncClient()
    who, a100, b200, lws = await asyncio.gather(
        kueue.whoami(),
        kueue.run(QUEUES["a100"], gpu_job("task-gpu-a100-@NONCE_SHORT@"), timeout=1200, grep="GPU "),
        kueue.run(QUEUES["b200"], gpu_job("task-gpu-b200-@NONCE_SHORT@"), timeout=1200, grep="GPU "),
        served(kueue),
    )
    result = {
        "kube_user": who["kube_user"],
        "a100": a100.strip(),
        "b200": b200.strip(),
        "served": lws,
    }
    ok = (
        "@NONCE@" in a100
        and "@NONCE@" in b200
        and "B200" in b200
        and lws["from_task"] != ""
        and lws["from_task"] in lws["from_job"]
    )
    print(json.dumps({**result, "pass": ok, "status": "pass" if ok else "fail"}))


asyncio.run(main())
