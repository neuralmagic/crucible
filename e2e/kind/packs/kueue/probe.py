"""Run a Job that prints a nonce and leaves it in a result file, fetch the file, and report
what the Job logged and who the task acted as."""

import asyncio
import json

from crucible_kueue import AsyncClient

JOB = {
    "apiVersion": "batch/v1",
    "kind": "Job",
    "metadata": {"name": "task-probe"},
    "spec": {
        "backoffLimit": 0,
        "template": {
            "spec": {
                "restartPolicy": "Never",
                "containers": [
                    {
                        "name": "probe",
                        "image": "@IMAGE@",
                        "command": [
                            "sh",
                            "-c",
                            "echo @NONCE@ && printf '{\"nonce\": \"@NONCE@\"}' > /results/result.json",
                        ],
                        "resources": {"requests": {"cpu": "100m", "memory": "64Mi"}},
                    }
                ],
            }
        },
    },
}



async def main() -> None:
    kueue = AsyncClient()
    who, log = await asyncio.gather(
        kueue.whoami(), kueue.run("@QUEUE@", JOB, timeout=240, results="/results")
    )
    collected = await kueue.results("Job", "task-probe", "out")
    print(
        json.dumps(
            {
                "kube_user": who["kube_user"],
                "launched_by": who["launched_by"],
                "nonce_logged": "@NONCE@" in log,
                "collected": collected,
            }
        )
    )


asyncio.run(main())
