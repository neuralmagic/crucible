"""Build vLLM at vllm_ref once through the run's own buildit, whose builds write the shared cache,
and fetch that build's logs as traces/seed.*. The last stdout line names the kernels the edit task
fans out over."""

import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

VLLM = "https://github.com/vllm-project/vllm.git"
LOGS = ["ninja_log", "ccache-stats.log", "steps.log"]


def buildit(*args: str) -> dict[str, object]:
    done = subprocess.run(["buildit", *args], stdout=subprocess.PIPE, text=True)
    lines = done.stdout.strip().splitlines()
    reply = json.loads(lines[-1]) if lines else {}
    if done.returncode != 0:
        sys.exit(f"buildit {args[0]} failed: {json.dumps(reply)}")
    return reply


def main() -> None:
    params = json.loads(Path(os.environ["CRUCIBLE_INPUTS_FILE"]).read_text())["params"]
    src = Path("build/vllm")
    git = ["git", "-C", str(src)]
    subprocess.run(["git", "init", "-q", str(src)], check=True)
    subprocess.run([*git, "fetch", "-q", "--depth", "1", VLLM, params["vllm_ref"]], check=True)
    subprocess.run([*git, "checkout", "-q", "--detach", "FETCH_HEAD"], check=True)

    kernels = [k.strip() for k in params["kernels"].split(",") if k.strip()]
    missing = [k for k in kernels if not (src / "csrc" / f"{k}_kernels.cu").is_file()]
    if not kernels or missing:
        sys.exit(f"kernels has no csrc/<name>_kernels.cu for {missing or 'an empty list'}")

    started = time.monotonic()
    built = buildit(
        "build",
        "-c", "build",
        "-f", "Dockerfile",
        "--ccache",
        "--cache", "vllm-ccache=/root/.cache/ccache",
        "--cache", "vllm-deps=/src/vllm/.deps",
        "--build-arg", f"BASE={params['base_image']}",
        "--build-arg", f"CUDA_ARCH_LIST={params['arch_list']}",
        "--build-arg", f"MAX_JOBS={params['max_jobs']}",
        "--timeout", "3600",
    )
    seconds = round(time.monotonic() - started)
    build_id = str(built["build_id"])
    buildit(
        "run", build_id, "--fetch", "/trace", "--",
        "sh", "-c",
        "mkdir -p /trace && cp /src/vllm/build/temp.*/.ninja_log /trace/ninja_log"
        " && cp /buildit/ccache-stats.log /buildit/steps.log /trace/",
    )
    fetched = Path(".buildit") / build_id / "trace"
    out = Path("traces")
    out.mkdir(exist_ok=True)
    for log in LOGS:
        shutil.copyfile(fetched / log, out / f"seed.{log}")
    print(json.dumps({"kernels": kernels, "seconds": seconds}))


if __name__ == "__main__":
    main()
