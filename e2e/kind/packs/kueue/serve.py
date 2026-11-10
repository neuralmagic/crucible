"""Serve a nonce over HTTP on Kueue, reach it from this task and from a Job, then delete it."""

import asyncio
import json
import urllib.error
import urllib.request

from crucible_kueue import AsyncClient, ToolError, WorkloadFailed, WorkloadStuck

RESOURCES = {"requests": {"cpu": "100m", "memory": "64Mi"}}
SERVER = {
    "apiVersion": "apps/v1",
    "kind": "Deployment",
    "metadata": {"name": "task-serve"},
    "spec": {
        "replicas": 1,
        "template": {
            "spec": {
                "containers": [
                    {
                        "name": "server",
                        "image": "@IMAGE@",
                        "command": [
                            "sh",
                            "-c",
                            "mkdir /tmp/www && cd /tmp/www && echo @NONCE@ > nonce"
                            " && exec python3 -m http.server 8000",
                        ],
                        "ports": [{"containerPort": 8000}],
                        "readinessProbe": {"httpGet": {"path": "/nonce", "port": 8000}},
                        "resources": RESOURCES,
                    }
                ]
            }
        },
    },
}


def client_job(url: str) -> dict:
    fetch = f"import urllib.request; print(urllib.request.urlopen('{url}', timeout=10).read().decode())"
    return {
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {"name": "task-client"},
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
                            "resources": RESOURCES,
                        }
                    ],
                }
            },
        },
    }


TICKS = 6
TICKER = {
    "apiVersion": "batch/v1",
    "kind": "Job",
    "metadata": {"name": "task-ticker"},
    "spec": {
        "backoffLimit": 0,
        "template": {
            "spec": {
                "restartPolicy": "Never",
                "containers": [
                    {
                        "name": "ticker",
                        "image": "@IMAGE@",
                        "command": [
                            "sh",
                            "-c",
                            f"for i in $(seq 1 {TICKS}); do echo tick-$i; sleep 1; done",
                        ],
                        "resources": RESOURCES,
                    }
                ],
            }
        },
    },
}


async def followed(kueue: AsyncClient) -> list[str]:
    """Every line the ticker writes, read in windows shorter than its run."""
    lines: list[str] = []
    async for _, line in kueue.follow_logs("Job", "task-ticker", window_seconds=2):
        lines.append(line.split(" ", 1)[1])
        if lines[-1] == f"tick-{TICKS}":
            break
    return lines


def one_shot(name: str, image: str, command: str) -> dict:
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
                            "name": "main",
                            "image": image,
                            "command": ["sh", "-c", command],
                            "resources": RESOURCES,
                        }
                    ],
                }
            },
        },
    }


async def failures(kueue: AsyncClient) -> dict:
    """What a Job that exits nonzero and a Job whose image cannot be pulled raise."""
    try:
        await kueue.run("@QUEUE@", one_shot("task-fails", "@IMAGE@", "echo doomed; exit 3"), timeout=240)
        failed = "returned"
    except WorkloadFailed as error:
        failed = {"reason": error.reason, "phase": error.body["phase"]}
    try:
        await kueue.run(
            "@QUEUE@", one_shot("task-unpullable", "localhost:1/nothing:here", "true"), timeout=240
        )
        stuck = "returned"
    except WorkloadStuck as error:
        stuck = {"problem_pods": len(error.body["problem_pods"])}
    await kueue.workload_delete("Job", "task-unpullable")
    return {"failed": failed, "stuck": stuck}


async def fetch(url: str) -> str:
    """The body at `url`, retried while the Service's endpoints settle."""
    last = ""
    for _ in range(30):
        try:
            with urllib.request.urlopen(url, timeout=5) as response:
                return response.read().decode().strip()
        except (urllib.error.URLError, OSError) as error:
            last = str(error)
            await asyncio.sleep(1)
    raise RuntimeError(f"{url} never answered: {last}")


async def gone(kueue: AsyncClient, kind: str, name: str) -> bool:
    for _ in range(60):
        try:
            await kueue.workload_status(kind, name)
        except ToolError as error:
            if error.status == "not_found":
                return True
            raise
        await asyncio.sleep(1)
    return False


async def main() -> None:
    kueue = AsyncClient()
    async with kueue.serving("@QUEUE@", SERVER, 8000, timeout=240) as url:
        again = await kueue.submit("@QUEUE@", SERVER, expose=[8000])
        status = await kueue.workload_status("Deployment", "task-serve")
        from_task = await fetch(f"{url}/nonce")
        from_job = await kueue.run("@QUEUE@", client_job(f"{url}/nonce"), timeout=240)
        try:
            await kueue.workload_plan("@QUEUE@", client_job(url), expose=[8000])
            batch_refused = False
        except ToolError as error:
            batch_refused = error.status == "refused"
    retired = await gone(kueue, "Deployment", "task-serve")
    await kueue.submit("@QUEUE@", TICKER)
    await kueue.wait("Job", "task-ticker", "running", timeout=240)
    ticks = await asyncio.wait_for(followed(kueue), timeout=120)
    raised = await failures(kueue)
    print(
        json.dumps(
            {
                "url": url,
                "resubmitted": again["submitted"],
                "status_service": status.get("service"),
                "from_task": from_task == "@NONCE@",
                "from_job": "@NONCE@" in from_job,
                "batch_refused": batch_refused,
                "ticks": ticks,
                "raised": raised,
                "gone": retired,
            }
        )
    )


asyncio.run(main())
