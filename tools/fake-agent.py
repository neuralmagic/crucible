#!/usr/bin/env python3
"""A deterministic stand-in for an agent turn, for packs and tests that must run with no model.

Point a manifest's `[agent] backend = "command"` / `agent_cmd` at this script and set
FAKE_AGENT_SCRIPT to a JSON file describing what each task does. The engine spawns a real
process, reads a real PLAN_TASK_RESULT.json, and sees a real exit code, so nothing about the
task boundary is simulated: only the model is absent.

Tasks are addressed by CRUCIBLE_TASK, the task's own name, not by matching prompt prose.

    {
      "draft":   {"result": {"entries": 3}, "writes": {"NOTES.md": "- one\\n"}},
      "audit":   {"reads": ["NOTES.md"], "result": {"findings": []}},
      "stale":   {"exit": 1, "stderr": "nothing to date the entries against"},
      "slow":    {"sleep_ms": 90000},
      "flaky":   {"fail_attempts": 1, "result": {"ok": true}}
    }

Directives, all optional:
  reads         paths that must exist before the turn does anything; absence exits 1 naming it,
                which is how a test proves a dependency's output was actually staged
  writes        path -> content, written before the result
  appends       path -> content, appended; use for proving one session spanned several turns
  sleep_ms      wall time to burn, for exercising deadlines and ceilings
  fail_attempts fail this many times before passing; the count is kept beside the workspace
  stderr        text to emit on stderr
  exit          exit code (default 0); a nonzero code skips the result file
  result        object written to PLAN_TASK_RESULT.json and echoed to stdout

Env the engine supplies, all readable from a template in `writes`/`result` as {ENV:NAME}:
  CRUCIBLE_TASK, CRUCIBLE_PROMPT, CRUCIBLE_INPUTS, CRUCIBLE_AGENT_SESSION,
  CRUCIBLE_AGENT_SESSION_ID, CRUCIBLE_AGENT_SESSION_ACTION
"""

import json
import os
import pathlib
import re
import sys
import time
from typing import Any, NoReturn

RESULT_FILE = "PLAN_TASK_RESULT.json"
ATTEMPTS_DIR = ".fake-agent"


def die(message: str) -> NoReturn:
    print(f"fake-agent: {message}", file=sys.stderr)
    raise SystemExit(1)


def expand(value: Any) -> Any:
    """Substitute {ENV:NAME} anywhere in a string, recursively through lists and objects."""
    if isinstance(value, str):
        return re.sub(
            r"\{ENV:([A-Z0-9_]+)\}", lambda m: os.environ.get(m.group(1), ""), value
        )
    if isinstance(value, list):
        return [expand(v) for v in value]
    if isinstance(value, dict):
        return {k: expand(v) for k, v in value.items()}
    return value


def attempt_number(task: str) -> int:
    """Count invocations per task so `fail_attempts` can fail the first N and then pass.

    The counter lives beside the workspace rather than in it, so a task that runs in a
    disposable worktree still sees a count that survives its own isolation.
    """
    root = pathlib.Path(os.environ.get("FAKE_AGENT_STATE", ATTEMPTS_DIR))
    root.mkdir(parents=True, exist_ok=True)
    counter = root / f"{task}.attempts"
    seen = int(counter.read_text().strip() or 0) if counter.exists() else 0
    counter.write_text(str(seen + 1))
    return seen + 1


def main() -> int:
    task = os.environ.get("CRUCIBLE_TASK", "")
    if not task:
        die("CRUCIBLE_TASK is unset: the engine did not name the task")

    script_path = os.environ.get("FAKE_AGENT_SCRIPT", "")
    if not script_path:
        die("FAKE_AGENT_SCRIPT is unset: nothing describes what this task should do")
    try:
        script: dict[str, dict[str, Any]] = json.loads(
            pathlib.Path(script_path).read_text()
        )
    except (OSError, ValueError) as exc:
        die(f"reading {script_path}: {exc}")

    if task not in script:
        # Silence here would let a renamed task pass by doing nothing at all.
        known = ", ".join(sorted(script)) or "none"
        die(f"no entry for task {task!r} in {script_path} (known: {known})")
    spec = script[task]

    reads: list[str] = expand(spec.get("reads", []))
    for required in reads:
        if not pathlib.Path(required).exists():
            die(f"{task}: {required} is absent, so it was never staged")

    writes: dict[str, str] = expand(spec.get("writes", {}))
    for path, content in writes.items():
        target = pathlib.Path(path)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)

    appends: dict[str, str] = expand(spec.get("appends", {}))
    for path, content in appends.items():
        target = pathlib.Path(path)
        target.parent.mkdir(parents=True, exist_ok=True)
        with target.open("a") as handle:
            handle.write(content)

    sleep_ms = int(spec.get("sleep_ms", 0))
    if sleep_ms:
        time.sleep(sleep_ms / 1000.0)

    fail_attempts = int(spec.get("fail_attempts", 0))
    if fail_attempts and attempt_number(task) <= fail_attempts:
        print(f"fake-agent: {task} failing attempt by request", file=sys.stderr)
        return 1

    if "stderr" in spec:
        print(expand(spec["stderr"]), file=sys.stderr)

    code = int(spec.get("exit", 0))
    if code != 0:
        # A failing turn writes no result: that is what the engine sees from a real one.
        return code

    if "result" in spec:
        payload = json.dumps(expand(spec["result"]), sort_keys=True)
        pathlib.Path(RESULT_FILE).write_text(payload)
        print(payload)
    return 0


if __name__ == "__main__":
    sys.exit(main())
