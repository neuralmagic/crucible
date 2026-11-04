#!/usr/bin/env python3
"""Replay a fixed trace of vLLM source edits through the run's buildit and time each build.

    replay.py step <name>  build one step of the trace and print its record
    replay.py report       write the step records to report.json and print the summary

A step builds `derive` when its compiled sources match the clean tree, `full` otherwise, and fails
unless the markers the build finds in its output are exactly the step's edits. informer.py names
the caches each build declares to buildit and reports, as the build's facts, what they held.

Stdlib only: it runs in the loop pod.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import time
import uuid
from dataclasses import asdict, dataclass
from pathlib import Path

VLLM_URL = "https://github.com/vllm-project/vllm.git"
TREE = "vllm"
STAGED = ".crucible-bench"
CONTAINERFILE = f"{STAGED}/Containerfile"
INFORMER = "informer.py"
BENCH_FILES = ("Containerfile", INFORMER)
MARKER = "crucible-bench-marker:"
# vLLM's WHEEL_BLOCKING_PATHS and WHEEL_WARNING_PATHS.
COMPILED_PATHS = (
    "csrc",
    "cmake",
    "CMakeLists.txt",
    "pyproject.toml",
    "vllm/_custom_ops.py",
    "rust",
    "setup.py",
)
SHA = re.compile(r"^[0-9a-f]{40}$")
TIMES = re.compile(r"^(?:\d+[:-])?crucible-bench-times\s+(.*)$")
FOUND = re.compile(r"^(?:\d+[:-])?crucible-bench-found(?:\s+(.*))?$")
LOG_GREP = r"^crucible-bench-(times|found)( |$)"
POLL_S = 15
# Status polls in a row that may fail before a build is given up on.
STATUS_TRIES = 8
# Builder and loop pod clocks may disagree by this much; a base step that started earlier than
# the build by more came from buildah's layer cache.
CLOCK_SLACK_S = 120
TIME_KEYS = ("base_start", "base_end", "compile_start", "compile_end")
GIT_REDIRECTS = (
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_COMMON_DIR",
)
SUMMARY_KEYS = (
    "step",
    "mode",
    "arch",
    "store",
    "ccache_hits",
    "ccache_misses",
    "base_cached",
    "wall_s",
    "candidate_s",
    "compile_s",
    "push_s",
)


class Fail(Exception):
    """A step or the report cannot be trusted."""


@dataclass(frozen=True)
class Edit:
    tag: str
    path: str
    cxx: bool

    def line(self) -> str:
        if self.cxx:
            return (
                "namespace crucible_bench { [[maybe_unused]] __attribute__((used)) inline const "
                f'char marker_{self.tag}[] = "{MARKER}{self.tag}"; }}\n'
            )
        return f"# {MARKER}{self.tag}\n"

    def apply(self, tree: Path) -> None:
        f = tree / self.path
        lines = f.read_text().splitlines(keepends=True)
        at = None
        if self.cxx:
            at = next((i + 1 for i, s in enumerate(lines) if s.strip() == "#pragma once"), None)
        if at is None:
            if lines and not lines[-1].endswith("\n"):
                lines[-1] += "\n"
            lines.append(self.line())
        else:
            lines.insert(at, self.line())
        f.write_text("".join(lines))


EDITS = {
    e.tag: e
    for e in (
        Edit("leaf", "csrc/layernorm_kernels.cu", cxx=True),
        Edit("header", "csrc/dispatch_utils.h", cxx=True),
        Edit("py", "vllm/logger.py", cxx=False),
        Edit("cmake", "CMakeLists.txt", cxx=False),
    )
}

STEPS: dict[str, tuple[str, ...]] = {
    "clean": (),
    "leaf": ("leaf",),
    "leaf_py": ("leaf", "py"),
    "revert": (),
    "leaf_again": ("leaf",),
    "header": ("header",),
    "cmake": ("cmake",),
}


def _str(raw: dict[str, object], key: str) -> str:
    v = raw.get(key)
    if not isinstance(v, str) or not v:
        raise Fail(f"param {key} is not a non-empty string: {v!r}")
    return v


def _int(raw: dict[str, object], key: str) -> int:
    v = raw.get(key)
    if not isinstance(v, int) or isinstance(v, bool) or v < 1:
        raise Fail(f"param {key} is not a positive int: {v!r}")
    return v


def _bool(raw: dict[str, object], key: str) -> bool:
    v = raw.get(key)
    if not isinstance(v, bool):
        raise Fail(f"param {key} is not a bool: {v!r}")
    return v


@dataclass(frozen=True)
class Params:
    base_image: str
    vllm_ref: str
    arch_list: str
    max_jobs: int
    push: bool
    timeout_s: int

    @staticmethod
    def parse(raw: object) -> Params:
        if not isinstance(raw, dict):
            raise Fail(f"params is not an object: {raw!r}")
        p = Params(
            base_image=_str(raw, "base_image"),
            vllm_ref=_str(raw, "vllm_ref"),
            arch_list=_str(raw, "arch_list"),
            max_jobs=_int(raw, "max_jobs"),
            push=_bool(raw, "push"),
            timeout_s=_int(raw, "timeout_s"),
        )
        if not SHA.match(p.vllm_ref):
            raise Fail(f"vllm_ref must be a full commit sha: {p.vllm_ref!r}")
        return p

    def build_args(self, mode: str, nonce: str) -> list[str]:
        args = {
            "BASE": self.base_image,
            "VLLM_REF": self.vllm_ref,
            "MODE": mode,
            "ARCH_LIST": self.arch_list,
            "JOBS": str(self.max_jobs),
            "MARKER_FILES": " ".join(sorted(e.path for e in EDITS.values() if not e.cxx)),
            "BUILD_NONCE": nonce,
        }
        return [a for k, v in args.items() for a in ("--build-arg", f"{k}={v}")]


def git(cwd: Path, *args: str) -> str:
    env = {k: v for k, v in os.environ.items() if k not in GIT_REDIRECTS}
    p = subprocess.run(["git", *args], cwd=cwd, env=env, capture_output=True, text=True)
    if p.returncode != 0:
        raise Fail(f"git {' '.join(args)} in {cwd}: {p.stderr.strip()}")
    return p.stdout.strip()


def head(tree: Path) -> str | None:
    if not (tree / ".git").is_dir():
        return None
    try:
        return git(tree, "rev-parse", "HEAD")
    except Fail:
        return None


def reset(tree: Path) -> None:
    git(tree, "checkout", "-q", "--", ".")
    git(tree, "clean", "-fdq")


def checkout(workspace: Path, ref: str, url: str = VLLM_URL) -> Path:
    tree = workspace / TREE
    if head(tree) != ref:
        shutil.rmtree(tree, ignore_errors=True)
        git(workspace, "init", "-q", TREE)
        git(tree, "fetch", "-q", "--depth", "1", url, ref)
        git(tree, "checkout", "-q", "--detach", "FETCH_HEAD")
    reset(tree)
    return tree


def stage_bench(tree: Path, bench: Path) -> None:
    """Put the Containerfile and the scripts it runs into the tree, under STAGED."""
    dest = tree / STAGED
    dest.mkdir(exist_ok=True)
    for name in BENCH_FILES:
        (dest / name).write_bytes((bench / name).read_bytes())
    exclude = tree / ".git" / "info" / "exclude"
    entry = f"/{STAGED}/"
    lines = exclude.read_text().splitlines() if exclude.exists() else []
    if entry not in lines:
        exclude.parent.mkdir(parents=True, exist_ok=True)
        exclude.write_text("\n".join([*lines, entry]) + "\n")


def compiled_digest(tree: Path) -> str:
    records = []
    for rel in COMPILED_PATHS:
        root = tree / rel
        if root.is_file():
            files = [root]
        elif root.is_dir():
            files = [p for p in root.rglob("*") if p.is_file()]
        else:
            records.append(f"M\0{rel}\0")
            continue
        for f in files:
            sha = hashlib.sha256(f.read_bytes()).hexdigest()
            records.append(f"F\0{f.relative_to(tree).as_posix()}\0{sha}\0")
    return hashlib.sha256("".join(sorted(records)).encode()).hexdigest()


def _pairs(text: str) -> dict[str, str]:
    return dict(kv.split("=", 1) for kv in text.split() if "=" in kv)


def _digits(raw: str | None) -> int | None:
    return int(raw) if raw is not None and raw.isdigit() else None


def parse_log(text: str) -> tuple[dict[str, int] | None, list[str] | None, dict[str, object]]:
    """The build's phase times, the markers it found, and the builder's arch and cpus."""
    times = found = None
    host: dict[str, object] = {}
    for line in text.splitlines():
        if m := TIMES.match(line):
            pairs = _pairs(m.group(1))
            if all(pairs.get(k, "").isdigit() for k in TIME_KEYS):
                times = {k: int(pairs[k]) for k in TIME_KEYS}
            host = {"arch": pairs.get("arch"), "cpus": _digits(pairs.get("cpus"))}
        elif m := FOUND.match(line):
            tokens = (m.group(1) or "").split()
            found = sorted(t.removeprefix(MARKER) for t in tokens if t.startswith(MARKER))
    return times, found, host


def cache_args(bench: Path) -> list[str]:
    """The informer's caches as buildit --cache flags."""
    p = subprocess.run(
        [sys.executable, str(bench / INFORMER), "caches"], capture_output=True, text=True
    )
    if p.returncode != 0:
        raise Fail(f"{INFORMER} caches exited {p.returncode}: {p.stderr.strip()[-2000:]}")
    caches = json.loads(p.stdout)
    if not isinstance(caches, list):
        raise Fail(f"{INFORMER} caches printed {caches!r}")
    out = []
    for c in caches:
        if not isinstance(c, dict) or not all(isinstance(c.get(k), str) for k in ("name", "path")):
            raise Fail(f"{INFORMER} caches printed a bad cache {c!r}")
        mode = ":ro" if c.get("read_only") is True else ""
        out += ["--cache", f"{c['name']}={c['path']}{mode}"]
    return out


def segments(times: dict[str, int], started: float, ended: float) -> dict[str, object]:
    """Split a build's wall time. A base step from buildah's layer cache kept the times of the
    build that ran it: it took no time here, and everything before the compile is setup."""
    ran = times["base_start"] >= started - CLOCK_SLACK_S
    base_s = float(times["base_end"] - times["base_start"]) if ran else 0.0
    return {
        "base_cached": not ran,
        "before_base_s": round(times["base_start"] - started, 1) if ran else None,
        "base_s": base_s,
        "copy_s": float(times["compile_start"] - times["base_end"]) if ran else None,
        "setup_s": round(times["compile_start"] - started - base_s, 1),
        "compile_s": float(times["compile_end"] - times["compile_start"]),
        "after_compile_s": round(ended - times["compile_end"], 1),
        "candidate_s": round(ended - started - base_s, 1),
    }


class Buildit:
    """`buildit` against the run's server, one JSON reply per call."""

    def __init__(self) -> None:
        if not os.environ.get("CRUCIBLE_BUILDIT_SOCKET"):
            raise Fail("CRUCIBLE_BUILDIT_SOCKET is unset: the run has no buildit")

    def call(self, *args: str) -> tuple[bool, dict[str, object]]:
        p = subprocess.run(["buildit", *args], capture_output=True, text=True)
        lines = [s for s in p.stdout.splitlines() if s.strip()]
        if p.returncode == 2 or not lines:
            raise Fail(f"buildit {args[0]} exited {p.returncode}: {p.stderr.strip()[-2000:]}")
        reply = json.loads(lines[-1])
        if not isinstance(reply, dict):
            raise Fail(f"buildit {args[0]} replied {reply!r}")
        return p.returncode == 0, reply

    def wait(self, build_id: str, limit_s: int) -> tuple[bool, dict[str, object]]:
        """Poll a build until it ends, riding out failed polls."""
        deadline = time.time() + limit_s + 600
        failed = 0
        while True:
            reply: dict[str, object]
            try:
                ok, reply = self.call("status", build_id)
            except Fail as e:
                ok = False
                reply = {"error": str(e)}
            if "status" in reply:
                failed = 0
                if reply["status"] != "running":
                    return ok, reply
            else:
                failed += 1
                print(f"polling {build_id}: {reply.get('error')}", file=sys.stderr)
                if failed >= STATUS_TRIES:
                    return False, reply
            if time.time() > deadline:
                return False, {"error": f"build {build_id} still running after {limit_s}s"}
            time.sleep(POLL_S)

    def clean(self, build_id: str) -> None:
        reply: dict[str, object]
        try:
            ok, reply = self.call("clean", build_id)
        except Fail as e:
            ok = False
            reply = {"error": str(e)}
        if not ok:
            print(f"cleaning {build_id}: {reply.get('error')}", file=sys.stderr)


def build(
    buildit: Buildit, params: Params, caches: list[str], mode: str, edits: tuple[str, ...]
) -> dict[str, object]:
    started = time.time()
    ok, reply = buildit.call(
        "build",
        "--no-wait",
        "-c",
        TREE,
        "-f",
        CONTAINERFILE,
        "--timeout",
        str(params.timeout_s),
        *caches,
        *params.build_args(mode, uuid.uuid4().hex),
    )
    build_id = str(reply.get("build_id", ""))
    if ok and build_id:
        ok, reply = buildit.wait(build_id, params.timeout_s)
    ended = time.time()
    record: dict[str, object] = {
        "build_id": build_id,
        "context_digest": reply.get("context_digest"),
        "wall_s": round(ended - started, 1),
    }
    try:
        if not ok:
            detail = reply.get("log_tail") or reply.get("error")
            raise Fail(f"build {build_id} ended {reply.get('status')}: {detail}")
        facts = reply.get("facts")
        if not isinstance(facts, dict):
            raise Fail(f"build {build_id} reported no facts: {reply}")
        ok, log = buildit.call("logs", build_id, "--grep", LOG_GREP)
        times, found, host = parse_log(str(log.get("lines", ""))) if ok else (None, None, {})
        if times is None or found is None:
            raise Fail(f"build {build_id}'s log has no times or markers: {log}")
        if found != sorted(edits):
            raise Fail(f"build {build_id} carries markers {found}, expected {sorted(edits)}")
        record |= segments(times, started, ended) | host | facts | {"found": found}
        if params.push:
            started = time.time()
            ok, pushed = buildit.call("push", build_id)
            if not ok:
                raise Fail(f"push of {build_id}: {pushed.get('error')}")
            record |= {"push_s": round(time.time() - started, 1), "image": pushed.get("image")}
    finally:
        if build_id:
            buildit.clean(build_id)
    return record


def resolve(tree: Path, edits: tuple[str, ...]) -> tuple[str, str]:
    """Apply `edits` to the clean tree; return the build mode and the compiled-sources digest."""
    clean = compiled_digest(tree)
    for tag in edits:
        EDITS[tag].apply(tree)
    compiled = compiled_digest(tree)
    return ("derive" if compiled == clean else "full"), compiled


def run_step(name: str, params: Params, workspace: Path, bench: Path) -> dict[str, object]:
    if name not in STEPS:
        raise Fail(f"no step {name!r}; steps are {', '.join(STEPS)}")
    edits = STEPS[name]
    buildit = Buildit()
    caches = cache_args(bench)
    tree = checkout(workspace, params.vllm_ref)
    stage_bench(tree, bench)
    try:
        mode, compiled = resolve(tree, edits)
        record = {"step": name, "edits": list(edits), "mode": mode, "compiled_digest": compiled}
        return record | build(buildit, params, caches, mode, edits)
    finally:
        reset(tree)


def report(inputs: dict[str, object], workspace: Path) -> dict[str, object]:
    params = Params.parse(inputs.get("params"))
    steps = []
    for name in STEPS:
        s = inputs.get(name)
        if not isinstance(s, dict) or not isinstance(s.get("candidate_s"), (int, float)):
            raise Fail(f"no record for step {name}: {s!r}")
        steps.append(s)
    total = round(sum(float(s["candidate_s"]) for s in steps), 1)
    full = {"path": "dockerfile", "params": asdict(params), "steps": steps, "candidate_s": total}
    (workspace / "report.json").write_text(json.dumps(full, indent=2) + "\n")
    return {
        "pass": True,
        "steps": [{k: s.get(k) for k in SUMMARY_KEYS} for s in steps],
        "candidate_s": total,
    }


def load_inputs() -> dict[str, object]:
    raw = json.loads(Path(os.environ["CRUCIBLE_INPUTS_FILE"]).read_text())
    if not isinstance(raw, dict):
        raise Fail("the inputs file is not an object")
    return raw


def main(argv: list[str]) -> int:
    workspace = Path.cwd()
    bench = Path(__file__).resolve().parent
    try:
        match argv:
            case ["step", name]:
                params = Params.parse(load_inputs().get("params"))
                out = run_step(name, params, workspace, bench)
            case ["report"]:
                out = report(load_inputs(), workspace)
            case _:
                print(__doc__, file=sys.stderr)
                return 2
    except Fail as e:
        print(json.dumps({"pass": False, "error": str(e)}))
        return 1
    print(json.dumps(out))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
