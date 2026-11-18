"""Check vLLM out at vllm_ref, build it once through the run's own buildit (whose builds write the
shared cache), push the image, and count what ccache served. The last stdout line emits the
pushed seed image, the same image by digest in the builder registry (what layer builds start
from, since builder pods pull nothing else), and the seed's build time and cache counts."""

import json
import os
import subprocess
import sys
import time
from collections import Counter
from pathlib import Path

VLLM = "https://github.com/vllm-project/vllm.git"
REMOTE_HITS = {"remote_storage_hit", "remote_storage_read_hit", "secondary_storage_hit"}
LOCAL_HITS = {"direct_cache_hit", "preprocessed_cache_hit"}


POLL_SECONDS = 20


def buildit(*args: str) -> dict[str, object]:
    done = subprocess.run(["buildit", *args], stdout=subprocess.PIPE, text=True)
    lines = done.stdout.strip().splitlines()
    reply = json.loads(lines[-1]) if lines else {}
    if done.returncode != 0:
        sys.exit(f"buildit {args[0]} failed: {json.dumps(reply)}")
    return reply


def build(*args: str) -> str:
    """Start a build and print its status and newest log line until it ends, so the task shows
    progress while it compiles. Returns the build id of a build that succeeded."""
    build_id = str(buildit("build", *args, "--no-wait")["build_id"])
    started = time.monotonic()
    last = ""
    while True:
        reply = buildit("status", build_id)
        tail = [line for line in str(reply.get("log_tail", "")).splitlines() if line.strip()]
        newest = tail[-1].strip() if tail else ""
        if newest and newest != last:
            print(f"[{round(time.monotonic() - started):>5}s] {newest[:200]}", flush=True)
            last = newest
        if reply.get("status") != "running":
            break
        time.sleep(POLL_SECONDS)
    if reply.get("status") != "succeeded":
        print("\n".join(tail[-40:]), flush=True)
        sys.exit(f"build {build_id} ended {reply.get('status')}: exit {reply.get('exit')}")
    return build_id


def ccache_counts(log: Path) -> dict[str, int]:
    """One outcome per compiler call in buildit's per-object ccache log."""
    outcomes: Counter[str] = Counter()
    counters: set[str] = set()

    def settle() -> None:
        if not counters:
            return
        if counters & REMOTE_HITS and "local_storage_hit" not in counters:
            outcomes["remote_hit"] += 1
        elif counters & LOCAL_HITS:
            outcomes["hit"] += 1
        elif "cache_miss" in counters:
            outcomes["miss"] += 1
        else:
            outcomes["uncached"] += 1
        counters.clear()

    for line in log.read_text().splitlines():
        if line.startswith("#"):
            settle()
        elif line.strip():
            counters.add(line.strip())
    settle()
    return {k: outcomes[k] for k in ("remote_hit", "hit", "miss", "uncached")}


def main() -> None:
    params = json.loads(Path(os.environ["CRUCIBLE_INPUTS_FILE"]).read_text())["params"]
    src = Path("build/vllm")
    git = ["git", "-C", str(src)]
    subprocess.run(["git", "init", "-q", str(src)], check=True)
    subprocess.run([*git, "fetch", "-q", "--depth", "1", VLLM, params["vllm_ref"]], check=True)
    subprocess.run([*git, "checkout", "-q", "--detach", "FETCH_HEAD"], check=True)

    started = time.monotonic()
    build_id = build(
        "-c", "build",
        "-f", "Dockerfile",
        "--ccache",
        "--cache", "vllm-ccache=/root/.cache/ccache",
        "--cache", "vllm-deps=/src/vllm/.deps:cluster",
        "--cache-key", "vllm-deps=vllm/CMakeLists.txt",
        "--cache-key", "vllm-deps=vllm/cmake/external_projects",
        "--build-arg", f"BASE={params['base_image']}",
        "--build-arg", f"CUDA_ARCH_LIST={params['arch_list']}",
        "--build-arg", f"MAX_JOBS={params['max_jobs']}",
        "--build-arg", f"SERVE_MODEL={params['serve_model']}",
        "--timeout", "3600",
    )
    build_seconds = round(time.monotonic() - started)
    pushed = buildit("push", build_id)
    push_seconds = round(time.monotonic() - started) - build_seconds
    ran = buildit(
        "run", build_id, "--fetch", "/trace", "--",
        "sh", "-c", "mkdir -p /trace && cp /buildit/ccache-stats.log /trace/",
    )
    stats = Path(".buildit") / build_id / "trace" / "ccache-stats.log"
    if stats.is_file():
        ccache: dict[str, int] | None = ccache_counts(stats)
    else:
        print(f"no ccache log came back from the run: {json.dumps(ran)[:2000]}", flush=True)
        ccache = None
    digest = str(pushed["image"]).split("@", 1)[1]
    print(
        json.dumps(
            {
                "seed_image": pushed["image"],
                "seed_base": f"{params['builder_registry']}@{digest}",
                "seed_build_id": build_id,
                "seed_build_seconds": build_seconds,
                "seed_push_seconds": push_seconds,
                "seed_ccache": ccache,
            }
        )
    )


if __name__ == "__main__":
    main()
