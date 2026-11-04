#!/usr/bin/env python3
"""A build informer for vLLM on vllm-envs: the caches a vLLM build declares, the key its compiled
outputs are stored under, and the build that fills and reuses them.

    informer.py caches                    the caches to declare to buildit, as JSON
    informer.py plan TREE --base IMAGE    the build key and env vllm-envs derives, as JSON
    informer.py build TREE --base IMAGE   build the tree's extensions; facts to /buildit/facts.json

`caches` is stdlib only and runs anywhere. `plan` and `build` run in the build, where vllm_envs
is installed. A full build restores the outputs stored under its key, or compiles through ccache
in a kept CMake build dir and stores them. When buildit names a shared cache in
/buildit/cache.json, ccache and the build store also read from it, and write to it when the build
holds the cache's token.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import asdict, dataclass
from pathlib import Path

FACTS = Path("/buildit/facts.json")
SHARED = Path("/buildit/cache.json")
HTTP_TIMEOUT_S = 60
CCACHE = Path("/root/.cache/ccache")
STORE = Path("/var/cache/vllm-builds")
CMAKE = Path("/var/cache/vllm-cmake")
ENVS = Path("/var/cache/vllm-envs")
SOURCES = ("csrc", "cmake", "CMakeLists.txt", "setup.py", "pyproject.toml", "vllm/_custom_ops.py")
SETTLED_MTIME = 946684800
CONFIGURED = re.compile(r"Configuring done \((\d+(?:\.\d+)?)s\)")
ERRORS = re.compile(r"FAILED:|error:|fatal error|CMake Error")
Facts = dict[str, object]


@dataclass(frozen=True)
class Cache:
    name: str
    path: Path
    read_only: bool = False


CACHES = (
    Cache("vllm-ccache", CCACHE),
    Cache("vllm-builds", STORE),
    Cache("vllm-cmake", CMAKE),
    Cache("vllm-envs", ENVS),
)


@dataclass(frozen=True)
class Plan:
    key: str
    env: dict[str, str]


@dataclass(frozen=True)
class Remote:
    """A shared `buildit cache`, and the token to write to it if this build has one."""

    url: str
    token: str | None

    @staticmethod
    def load(doc: Path) -> Remote | None:
        try:
            raw = json.loads(doc.read_text())
        except (OSError, ValueError):
            return None
        url = raw.get("url") if isinstance(raw, dict) else None
        if not isinstance(url, str) or not url.startswith("http://"):
            return None
        token = raw.get("token")
        return Remote(url.rstrip("/"), token if isinstance(token, str) and token else None)

    def ccache_storage(self) -> str:
        """ccache's remote storage URL: the token as a basic-auth password, or read-only."""
        if self.token is None:
            return f"{self.url}/ccache|read-only"
        parts = urllib.parse.urlsplit(self.url)
        netloc = f"informer:{urllib.parse.quote(self.token, safe='')}@{parts.netloc}"
        return urllib.parse.urlunsplit(parts._replace(netloc=netloc)) + "/ccache"

    def fetch(self, name: str, dest: Path) -> bool:
        """Download store/NAME to DEST. False when the cache lacks it or cannot be reached."""
        part = dest.with_name(dest.name + ".part")
        try:
            with urllib.request.urlopen(f"{self.url}/store/{name}", timeout=HTTP_TIMEOUT_S) as r:
                with part.open("wb") as out:
                    shutil.copyfileobj(r, out, 1 << 20)
        except urllib.error.HTTPError as e:
            e.close()
            part.unlink(missing_ok=True)
            if e.code != 404:
                print(f"informer: fetching store/{name}: {e}", file=sys.stderr)
            return False
        except OSError as e:
            part.unlink(missing_ok=True)
            print(f"informer: fetching store/{name}: {e}", file=sys.stderr)
            return False
        part.rename(dest)
        return True

    def publish(self, name: str, src: Path) -> str:
        """Upload SRC as store/NAME: `stored`, `read-only`, or why it failed."""
        if self.token is None:
            return "read-only"
        headers = {
            "Authorization": f"Bearer {self.token}",
            "Content-Length": str(src.stat().st_size),
            "Content-Type": "application/octet-stream",
        }
        try:
            with src.open("rb") as f:
                req = urllib.request.Request(f"{self.url}/store/{name}", f, headers, method="PUT")
                with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT_S):
                    return "stored"
        except urllib.error.HTTPError as e:
            e.close()
            return f"failed: {e}"
        except OSError as e:
            return f"failed: {e}"


class Failed(Exception):
    """The build failed; the message is what to read first."""


def run(cmd: list[str], cwd: Path) -> str:
    return subprocess.run(cmd, cwd=cwd, check=True, text=True, capture_output=True).stdout


def git_files(tree: Path, *args: str) -> list[str]:
    return [p for p in run(["git", "ls-files", "-z", *args], tree).split("\0") if p]


def plan(tree: Path, base: str, arch_list: str) -> Plan:
    """vllm-envs' key for the tree's compiled outputs, with the base image folded in, and the
    pinned external sources as *_SRC_DIR, cloned once per pin under VE_CACHE_DIR."""
    os.environ.setdefault("VE_CACHE_DIR", str(ENVS))
    os.environ["TORCH_CUDA_ARCH_LIST"] = arch_list
    from vllm_envs.config import load_config
    from vllm_envs.extprojects import src_dir_env
    from vllm_envs.hashing import build_key

    env = {str(k): str(v) for k, v in src_dir_env(load_config(), tree).items()}
    python = f"{sys.version_info.major}.{sys.version_info.minor}"
    key = build_key(tree, "cuda", python, Path(sys.prefix))
    return Plan(hashlib.sha256(f"{key}|{base}".encode()).hexdigest()[:32], env)


def manifest(tree: Path) -> dict[str, str]:
    """Source path -> content hash, over the tracked sources a compile reads."""
    paths = git_files(tree, *SOURCES)
    return {p: hashlib.sha256((tree / p).read_bytes()).hexdigest() for p in paths if (tree / p).is_file()}


def touch_changed(tree: Path, last: dict[str, str], now: dict[str, str]) -> list[str]:
    """Date every source by content against the last build's manifest: changed sources now,
    unchanged ones long before any object, so ninja rebuilds exactly what changed however the
    tree's mtimes arrived."""
    if not last:
        return []
    changed = sorted(p for p, h in now.items() if last.get(p) != h)
    for p in now:
        if last.get(p) == now[p]:
            os.utime(tree / p, (SETTLED_MTIME, SETTLED_MTIME))
    for p in changed:
        (tree / p).touch()
    return changed


def tar_into(archive: Path, tree: Path, paths: list[str]) -> None:
    part = archive.with_name(archive.name + ".part")
    with tarfile.open(part, "w") as tar:
        for p in paths:
            tar.add(tree / p, arcname=p, recursive=False)
    part.rename(archive)


def untar(archive: Path, tree: Path, keep_existing: bool) -> None:
    with tarfile.open(archive) as tar:
        members = [m for m in tar if not (keep_existing and (tree / m.name).exists())]
        tar.extractall(tree, members=members, filter="data")


def written_since(tree: Path, under: str, stamp: Path) -> list[str]:
    since = stamp.stat().st_mtime_ns
    return sorted(
        p.relative_to(tree).as_posix()
        for p in (tree / under).rglob("*")
        if p.is_file() and not p.is_symlink() and p.stat().st_mtime_ns > since
    )


def save_generated(tree: Path, saved: Path) -> None:
    """Keep the sources CMake generated into csrc: the kept build dir records them as done."""
    tar_into(saved, tree, git_files(tree, "--others", "csrc"))


def restore_generated(tree: Path, saved: Path) -> None:
    if saved.exists():
        untar(saved, tree, keep_existing=True)


def ccache_counts(stats: str) -> Facts:
    """Hits, misses and the hits served from remote storage, from `ccache --print-stats`."""
    values: dict[str, int] = {}
    for line in stats.splitlines():
        name, _, value = line.partition("\t")
        if value.strip().isdigit():
            values[name] = int(value)
    return {
        "ccache_hits": values.get("direct_cache_hit", 0) + values.get("preprocessed_cache_hit", 0),
        "ccache_misses": values.get("cache_miss", 0),
        "ccache_remote_hits": values.get("remote_storage_hit", 0)
        + values.get("secondary_storage_hit", 0),
    }


def compile_tree(
    tree: Path, arch_list: str, jobs: int, env: dict[str, str], scratch: Path, remote: Remote | None
) -> Facts:
    (CMAKE / "deps").mkdir(parents=True, exist_ok=True)
    if remote is not None:
        os.environ["CCACHE_REMOTE_STORAGE"] = os.environ["CCACHE_SECONDARY_STORAGE"] = (
            remote.ccache_storage()
        )
    os.environ |= env | {
        "CCACHE_DIR": str(CCACHE),
        "CCACHE_BASEDIR": str(tree),
        "CCACHE_NOHASHDIR": "true",
        "CCACHE_COMPILERCHECK": "content",
        "CCACHE_MAXSIZE": "50G",
        "VLLM_DISABLE_SCCACHE": "1",
        "FETCHCONTENT_BASE_DIR": str(CMAKE / "deps"),
        "TORCH_CUDA_ARCH_LIST": arch_list,
        "MAX_JOBS": str(jobs),
    }
    run(["ccache", "-z"], tree)
    generated = CMAKE / "generated.tar"
    restore_generated(tree, generated)
    saved = CMAKE / "manifest.json"
    now = manifest(tree)
    touched = touch_changed(tree, json.loads(saved.read_text()) if saved.exists() else {}, now)
    log = scratch / "build.log"
    with log.open("w") as out:
        built = subprocess.run(
            ["python3", "setup.py", "build_ext", "--inplace", "--build-temp", str(CMAKE / "build")],
            cwd=tree,
            stdout=out,
            stderr=subprocess.STDOUT,
        )
    lines = log.read_text(errors="replace").splitlines()
    if built.returncode != 0:
        errors = [s for s in lines if ERRORS.search(s) and "deprecat" not in s.lower()]
        raise Failed("\n".join(errors[:25] + lines[-20:]))
    saved.write_text(json.dumps(now))
    save_generated(tree, generated)
    configured = [float(m) for s in lines for m in CONFIGURED.findall(s)]
    return ccache_counts(run(["ccache", "--print-stats"], tree)) | {
        "configure_s": configured[-1] if configured else None,
        "touched": len(touched),
    }


def restore(tree: Path, stored: Path, remote: Remote | None) -> str | None:
    """Restore the outputs stored at STORED, from the local store or the shared one: `hit`,
    `remote-hit`, or None when neither has them."""
    if stored.exists():
        untar(stored, tree, keep_existing=False)
        return "hit"
    if remote is not None and remote.fetch(stored.name, stored):
        untar(stored, tree, keep_existing=False)
        return "remote-hit"
    return None


def build(
    tree: Path, base: str, arch_list: str, jobs: int, mode: str, remote: Remote | None
) -> Facts:
    started = time.time()
    facts: Facts = {"mode": mode, "store": "off", "shared": shared_mode(remote)}
    if mode == "full":
        planned = plan(tree, base, arch_list)
        stored = STORE / f"{planned.key}.tar"
        facts |= {"key": planned.key, "src_dirs": sorted(planned.env)}
        STORE.mkdir(parents=True, exist_ok=True)
        if restored := restore(tree, stored, remote):
            facts["store"] = restored
        else:
            with tempfile.TemporaryDirectory() as tmp:
                scratch = Path(tmp)
                stamp = scratch / "stamp"
                stamp.touch()
                facts |= compile_tree(tree, arch_list, jobs, planned.env, scratch, remote)
                outputs = written_since(tree, "vllm", stamp)
            tar_into(stored, tree, outputs)
            facts |= {"store": "miss", "outputs": len(outputs)}
            if remote is not None:
                facts["published"] = remote.publish(stored.name, stored)
    facts["build_s"] = round(time.time() - started, 1)
    return facts


def shared_mode(remote: Remote | None) -> str:
    if remote is None:
        return "off"
    return "read-write" if remote.token else "read-only"


def write_facts(facts: Facts) -> None:
    if FACTS.parent.is_dir():
        FACTS.write_text(json.dumps(facts) + "\n")
    print("informer-facts " + json.dumps(facts, separators=(",", ":")))


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="informer.py")
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("caches")
    for name in ("plan", "build"):
        cmd = sub.add_parser(name)
        cmd.add_argument("tree", type=Path)
        cmd.add_argument("--base", required=True)
        cmd.add_argument("--arch-list", default=os.environ.get("TORCH_CUDA_ARCH_LIST", ""))
        if name == "build":
            cmd.add_argument("--jobs", type=int, default=os.cpu_count() or 1)
            cmd.add_argument("--mode", choices=("full", "derive"), default="full")
    args = parser.parse_args(argv)
    match args.command:
        case "caches":
            print(json.dumps([asdict(c) | {"path": str(c.path)} for c in CACHES]))
        case "plan":
            print(json.dumps(asdict(plan(args.tree, args.base, args.arch_list))))
        case _:
            try:
                remote = Remote.load(SHARED)
                facts = build(args.tree, args.base, args.arch_list, args.jobs, args.mode, remote)
                write_facts(facts)
            except Failed as e:
                print(e, file=sys.stderr)
                write_facts({"mode": args.mode, "error": str(e)[-4000:]})
                return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
