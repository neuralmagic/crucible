#!/usr/bin/env python3
"""Verify an optimize-loop run against evidence instead of the agent's report.

    verify.py gate      the optimizer's report is well formed (it names no evidence yet)
    verify.py verify    the report agrees with what crucible-kueue and the registry say

`verify` reads, under the workspace:

    inputs/optimize/report.json         what the optimizer says it did
    inputs/optimize/<harness>           the harness it built from, against the pack's own copy
    inputs/collect/evidence/kueue.json  crucible-kueue's replies, gathered by a second agent

and the push repository from the run's buildit grant (CRUCIBLE_AGENT_SERVERS), whose manifests
and layers it reads with the loop pod's push authfile (FORGE_AUTHFILE). It prints one JSON line
the playbook's evaluate task reads: `pass`, `score` (the speedup it computed), and why.

Stdlib only: it runs in the loop pod.
"""

from __future__ import annotations

import base64
import gzip
import hashlib
import io
import json
import os
import re
import socket
import sys
import tarfile
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Protocol

RESULT_PREFIX = "optimize-loop-result "
RECORDS = 200_000
USERS = 5_000
TOP_K = 20
ITERATIONS = 5
BASE_SEED = 0x6F70746C6F6F70
GPU_ROWS = 2 * 8 * 512
MASK64 = (1 << 64) - 1

# A claimed number may differ from the evidence by this fraction (float formatting, rounding).
CLAIM_TOLERANCE = 0.01
# Seconds of clock skew allowed between a log's own timings and the Job's start and completion.
WALL_SLACK_S = 3.0
MAX_BLOB_BYTES = 256 * 1024 * 1024

JOB_NAME = re.compile(r"^[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?$")
DIGEST = re.compile(r"^sha256:[0-9a-f]{64}$")
MANIFEST_TYPES = (
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
)
INDEX_TYPES = (
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
)
HARNESS = {"cpu": "app/main.go", "gpu": "gpu/bench.py"}
BINARY = {"cpu": "bench"}


class Invalid(Exception):
    """The report or the evidence is malformed or disagrees with itself."""


# --- the reference answer ------------------------------------------------------------------


def fnv1a64(text: str) -> int:
    h = 0xCBF29CE484222325
    for byte in text.encode():
        h ^= byte
        h = (h * 0x100000001B3) & MASK64
    return h


def pod_seed(pod: str) -> int:
    return BASE_SEED ^ fnv1a64(pod)


def reference_checksum(seed: int, records: int) -> str:
    """What app/main.go prints for `seed`, computed the obvious way."""
    state = seed
    counts: dict[int, list[int]] = {}

    def nxt() -> int:
        nonlocal state
        state = (state + 0x9E3779B97F4A7C15) & MASK64
        z = state
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & MASK64
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & MASK64
        return z ^ (z >> 31)

    for _ in range(records):
        user = nxt() % USERS
        moved = nxt() % 4096
        entry = counts.setdefault(user, [0, 0])
        entry[0] += 1
        entry[1] += moved
    ranked = sorted(
        ((f"user-{u:05d}", c, b) for u, (c, b) in counts.items()),
        key=lambda t: (-t[1], -t[2], t[0]),
    )[:TOP_K]
    text = "".join(f"{u} {c} {b}\n" for u, c, b in ranked)
    return hashlib.sha256(text.encode()).hexdigest()


# --- inputs ----------------------------------------------------------------------------------


@dataclass(frozen=True)
class Params:
    variant: str
    rounds: int
    min_speedup: float

    @staticmethod
    def parse(raw: dict[str, object]) -> Params:
        """The workflow's launch params as the inputs file carries them, in their declared types."""
        gpu = raw.get("gpu", False)
        rounds = raw.get("rounds", 3)
        min_speedup = raw.get("min_speedup", 5)
        if not isinstance(gpu, bool):
            raise Invalid(f"param gpu {gpu!r} is not a bool")
        if isinstance(rounds, bool) or not isinstance(rounds, int):
            raise Invalid(f"param rounds {rounds!r} is not an integer")
        if isinstance(min_speedup, bool) or not isinstance(min_speedup, (int, float)):
            raise Invalid(f"param min_speedup {min_speedup!r} is not a number")
        return Params(variant="gpu" if gpu else "cpu", rounds=rounds, min_speedup=min_speedup)


@dataclass(frozen=True)
class Claim:
    round: int
    job: str
    image: str
    checksum: str
    ops_per_s: float
    p50_ms: float


@dataclass(frozen=True)
class Report:
    jobs: tuple[Claim, ...]
    best_job: str
    speedup: float

    @staticmethod
    def parse(raw: object) -> Report:
        if not isinstance(raw, dict):
            raise Invalid("report.json is not an object")
        jobs = raw.get("jobs")
        if not isinstance(jobs, list) or not jobs:
            raise Invalid("report.json lists no jobs")
        claims = []
        for i, j in enumerate(jobs):
            if not isinstance(j, dict):
                raise Invalid(f"jobs[{i}] is not an object")
            try:
                claim = Claim(
                    round=_int(j["round"]),
                    job=_str(j["job"]),
                    image=_str(j["image"]),
                    checksum=_str(j["checksum"]),
                    ops_per_s=_num(j["ops_per_s"]),
                    p50_ms=_num(j["p50_ms"]),
                )
            except (KeyError, TypeError) as e:
                raise Invalid(f"jobs[{i}]: missing or mistyped {e}") from e
            if not JOB_NAME.match(claim.job):
                raise Invalid(f"jobs[{i}]: {claim.job!r} is not a Job name")
            claims.append(claim)
        names = [c.job for c in claims]
        if len(set(names)) != len(names):
            raise Invalid("report.json names a Job twice")
        try:
            best_job = _str(raw["best_job"])
            speedup = _num(raw["speedup"])
        except (KeyError, TypeError) as e:
            raise Invalid(f"report.json: missing or mistyped {e}") from e
        if best_job not in names:
            raise Invalid(f"best_job {best_job!r} is not one of the reported jobs")
        return Report(jobs=tuple(claims), best_job=best_job, speedup=speedup)


def _int(v: object) -> int:
    if isinstance(v, bool) or not isinstance(v, int):
        raise TypeError(repr(v))
    return v


def _num(v: object) -> float:
    if isinstance(v, bool) or not isinstance(v, (int, float)):
        raise TypeError(repr(v))
    return float(v)


def _str(v: object) -> str:
    if not isinstance(v, str):
        raise TypeError(repr(v))
    return v


@dataclass(frozen=True)
class Observed:
    """One Job as crucible-kueue reported it."""

    name: str
    run: str | None
    succeeded: bool
    started: datetime | None
    completed: datetime | None
    pod: str | None
    result: dict[str, object] | None

    @property
    def wall_s(self) -> float | None:
        if self.started is None or self.completed is None:
            return None
        return (self.completed - self.started).total_seconds()


def parse_evidence(raw: object) -> dict[str, Observed]:
    """The collector's file: `list` is workload_list's reply, `jobs[name]` holds that Job's
    workload_status and workload_logs replies."""
    if not isinstance(raw, dict):
        raise Invalid("evidence is not an object")
    listed = raw.get("list")
    jobs = raw.get("jobs")
    if not isinstance(listed, dict) or not isinstance(listed.get("workloads"), list):
        raise Invalid("evidence has no workload_list reply")
    if not isinstance(jobs, dict):
        raise Invalid("evidence has no per-Job replies")
    runs: dict[str, str | None] = {}
    for w in listed["workloads"]:
        if isinstance(w, dict) and w.get("kind") == "Job" and isinstance(w.get("name"), str):
            run = w.get("run")
            runs[w["name"]] = run if isinstance(run, str) else None
    out = {}
    for name, replies in jobs.items():
        if not isinstance(replies, dict):
            raise Invalid(f"evidence for {name} is not an object")
        status = replies.get("status")
        logs = replies.get("logs")
        if not isinstance(status, dict) or not isinstance(logs, dict):
            raise Invalid(f"evidence for {name} lacks a status or logs reply")
        obj = status.get("object_status")
        obj = obj if isinstance(obj, dict) else {}
        succeeded = isinstance(obj.get("succeeded"), int) and obj["succeeded"] >= 1
        pod, result = _result_line(logs)
        out[name] = Observed(
            name=name,
            run=runs.get(name),
            succeeded=succeeded,
            started=_time(obj.get("startTime")),
            completed=_time(obj.get("completionTime")),
            pod=pod,
            result=result,
        )
    return out


def _time(v: object) -> datetime | None:
    if not isinstance(v, str):
        return None
    try:
        return datetime.fromisoformat(v.replace("Z", "+00:00"))
    except ValueError:
        return None


def _result_line(logs: dict[str, object]) -> tuple[str | None, dict[str, object] | None]:
    """The pod and parsed result line of a single-pod Job's logs, or Nones."""
    entries = logs.get("logs")
    if not isinstance(entries, list) or len(entries) != 1 or not isinstance(entries[0], dict):
        return None, None
    entry = entries[0]
    pod = entry.get("pod") if isinstance(entry.get("pod"), str) else None
    text = entry.get("log")
    if not isinstance(text, str):
        return pod, None
    found = [ln[len(RESULT_PREFIX) :] for ln in text.splitlines() if ln.startswith(RESULT_PREFIX)]
    if len(found) != 1:
        return pod, None
    try:
        parsed = json.loads(found[0])
    except json.JSONDecodeError:
        return pod, None
    return pod, parsed if isinstance(parsed, dict) else None


# --- the registry ----------------------------------------------------------------------------


class Registry(Protocol):
    def manifest(self, digest: str) -> bytes: ...
    def blob(self, digest: str) -> bytes: ...


@dataclass(frozen=True)
class Image:
    repo: str
    digest: str

    @staticmethod
    def parse(ref: str, push_repo: str) -> Image:
        repo, sep, digest = ref.partition("@")
        if not sep or not DIGEST.match(digest):
            raise Invalid(f"{ref} is not an image by digest")
        if repo != push_repo:
            raise Invalid(f"{ref} is not in the run's push repository {push_repo}")
        return Image(repo=repo, digest=digest)


def _sha256(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def image_files(registry: Registry, image: Image, wanted: str) -> str | None:
    """The sha256 of file `wanted` in the image's top-most layer holding it, or None. Raises
    Invalid when the registry does not hold the image or serves content of another digest."""
    raw = registry.manifest(image.digest)
    if _sha256(raw) != image.digest:
        raise Invalid(f"{image.repo}@{image.digest}: the registry served another manifest")
    manifest = json.loads(raw)
    if manifest.get("mediaType") in INDEX_TYPES or "manifests" in manifest:
        picked = [
            m
            for m in manifest.get("manifests", [])
            if m.get("platform", {}).get("os") == "linux"
            and m.get("platform", {}).get("architecture") == "amd64"
        ]
        if not picked:
            raise Invalid(f"{image.repo}@{image.digest} has no linux/amd64 image")
        return image_files(registry, Image(image.repo, picked[0]["digest"]), wanted)
    found = None
    for layer in manifest.get("layers", []):
        digest = layer.get("digest", "")
        if not DIGEST.match(digest):
            raise Invalid(f"{image.repo}@{image.digest} names a malformed layer")
        data = registry.blob(digest)
        if _sha256(data) != digest:
            raise Invalid(f"layer {digest} does not hash to its digest")
        hashed = _hash_member(data, wanted)
        if hashed is not None:
            found = hashed
    return found


def _hash_member(layer: bytes, wanted: str) -> str | None:
    if layer[:2] == b"\x1f\x8b":
        layer = gzip.decompress(layer)
    with tarfile.open(fileobj=io.BytesIO(layer), mode="r:") as tar:
        for member in tar:
            if member.isfile() and member.name.lstrip("./") == wanted:
                f = tar.extractfile(member)
                if f is not None:
                    return hashlib.sha256(f.read()).hexdigest()
    return None


class LayoutRegistry:
    """An OCI image layout on disk, as `skopeo copy ... oci:DIR` writes it."""

    def __init__(self, root: Path) -> None:
        self.root = root

    def _read(self, digest: str) -> bytes:
        if not DIGEST.match(digest):
            raise Invalid(f"{digest} is not a sha256 digest")
        path = self.root / "blobs" / "sha256" / digest.removeprefix("sha256:")
        try:
            return path.read_bytes()
        except FileNotFoundError as e:
            raise Invalid(f"{digest} is not in {self.root}") from e

    def manifest(self, digest: str) -> bytes:
        return self._read(digest)

    def blob(self, digest: str) -> bytes:
        return self._read(digest)


def basic_auth(authfile: dict[str, object], repo: str) -> str | None:
    """The base64 user:password the authfile holds for `repo`: the longest `auths` key that is
    the repo's host or a path prefix of it."""
    auths = authfile.get("auths")
    if not isinstance(auths, dict):
        return None
    best: tuple[int, str] | None = None
    for key, entry in auths.items():
        k = re.sub(r"^https?://", "", key).rstrip("/")
        if repo == k or repo.startswith(k + "/"):
            if isinstance(entry, dict) and isinstance(entry.get("auth"), str):
                if best is None or len(k) > best[0]:
                    best = (len(k), entry["auth"])
    return best[1] if best else None


def parse_challenge(header: str) -> tuple[str, dict[str, str]]:
    """`Bearer realm="...",service="..."` as ("bearer", {realm, service})."""
    scheme, _, rest = header.strip().partition(" ")
    params = {k.lower(): v for k, v in re.findall(r'(\w+)="([^"]*)"', rest)}
    return scheme.lower(), params


class HttpRegistry:
    """The distribution API of the push repository's registry."""

    def __init__(self, repo: str, auth: str | None) -> None:
        self.host, _, self.path = repo.partition("/")
        self.auth = auth
        self.header: str | None = None

    def _get(self, url: str, accept: str) -> bytes:
        for _ in range(2):
            req = urllib.request.Request(url, headers={"Accept": accept})
            if self.header:
                req.add_unredirected_header("Authorization", self.header)
            try:
                with urllib.request.urlopen(req, timeout=60) as resp:
                    data = resp.read(MAX_BLOB_BYTES + 1)
                    if len(data) > MAX_BLOB_BYTES:
                        raise Invalid(f"{url} is larger than {MAX_BLOB_BYTES} bytes")
                    return data
            except urllib.error.HTTPError as e:
                if e.code == 401 and self.header is None:
                    self.header = self._authorize(e.headers.get("WWW-Authenticate", ""))
                    continue
                if e.code == 404:
                    raise Invalid(f"{url}: not in the registry") from e
                raise Invalid(f"{url}: HTTP {e.code}") from e
        raise Invalid(f"{url}: the registry refused the credentials")

    def _authorize(self, challenge: str) -> str:
        scheme, params = parse_challenge(challenge)
        if scheme == "basic":
            if not self.auth:
                raise Invalid(f"{self.host} wants credentials and the authfile has none")
            return "Basic " + self.auth
        if scheme != "bearer" or "realm" not in params:
            raise Invalid(f"{self.host}: unsupported challenge {challenge!r}")
        query = {"scope": f"repository:{self.path}:pull"}
        if "service" in params:
            query["service"] = params["service"]
        req = urllib.request.Request(params["realm"] + "?" + urllib.parse.urlencode(query))
        if self.auth:
            req.add_unredirected_header("Authorization", "Basic " + self.auth)
        try:
            with urllib.request.urlopen(req, timeout=60) as resp:
                body = json.loads(resp.read())
        except (urllib.error.URLError, json.JSONDecodeError) as e:
            raise Invalid(f"{self.host}: token request failed: {e}") from e
        token = body.get("token") or body.get("access_token")
        if not isinstance(token, str):
            raise Invalid(f"{self.host}: the token endpoint returned no token")
        return "Bearer " + token

    def manifest(self, digest: str) -> bytes:
        return self._get(
            f"https://{self.host}/v2/{self.path}/manifests/{digest}",
            ", ".join(MANIFEST_TYPES + INDEX_TYPES),
        )

    def blob(self, digest: str) -> bytes:
        return self._get(f"https://{self.host}/v2/{self.path}/blobs/{digest}", "*/*")


def push_repo_from_env(raw: str) -> str:
    """The push repository of the run's buildit grant in CRUCIBLE_AGENT_SERVERS."""
    try:
        servers = json.loads(raw) if raw.strip() else []
    except json.JSONDecodeError as e:
        raise Invalid(f"CRUCIBLE_AGENT_SERVERS is not JSON: {e}") from e
    for s in servers if isinstance(servers, list) else []:
        transport = s.get("transport") if isinstance(s, dict) else None
        if isinstance(transport, dict):
            site = transport.get("local", {}).get("buildit")
            if isinstance(site, dict) and isinstance(site.get("push_repo"), str):
                return site["push_repo"]
    raise Invalid("the run has no buildit grant with a push repository")


# --- the verdict -----------------------------------------------------------------------------


@dataclass(frozen=True)
class Context:
    params: Params
    push_repo: str
    run: str
    records: int = RECORDS
    harness_tampered: bool = False


@dataclass
class Verdict:
    failures: list[str] = field(default_factory=list)
    rejected: dict[str, str] = field(default_factory=dict)
    speedup: float | None = None
    baseline: dict[str, object] | None = None
    best: dict[str, object] | None = None

    @property
    def passed(self) -> bool:
        return not self.failures

    def line(self) -> dict[str, object]:
        return {
            "pass": self.passed,
            "score": self.speedup if self.speedup is not None else 0.0,
            "speedup": self.speedup if self.speedup is not None else 0.0,
            "baseline": self.baseline or {},
            "best": self.best or {},
            "rejected": self.rejected,
            "failures": self.failures,
        }


def _close(claimed: float, observed: float) -> bool:
    return abs(claimed - observed) <= CLAIM_TOLERANCE * max(abs(observed), 1e-9)


def verify(
    report: Report, evidence: dict[str, Observed], ctx: Context, registry: Registry
) -> Verdict:
    v = Verdict()
    p = ctx.params
    if ctx.harness_tampered:
        v.failures.append(f"{HARNESS[p.variant]} differs from the pack's copy")
    baseline = [c for c in report.jobs if c.round == 0]
    if len(baseline) != 1:
        v.failures.append(f"round 0 must hold exactly one baseline Job, it holds {len(baseline)}")
    rounds = sorted({c.round for c in report.jobs if c.round != 0})
    if not rounds or rounds[0] < 1 or rounds[-1] > p.rounds:
        v.failures.append(f"optimization rounds {rounds} are not within 1..{p.rounds}")

    measured: dict[str, dict[str, object]] = {}
    for c in report.jobs:
        problem = _check_job(c, evidence.get(c.job), ctx, registry)
        if problem is not None:
            v.failures.append(f"{c.job}: {problem}")
            continue
        observed = evidence[c.job].result or {}
        if observed.get("checksum") != _expected_checksum(evidence[c.job], baseline, evidence, p):
            v.rejected[c.job] = "wrong answer"
            continue
        measured[c.job] = observed
    if v.failures:
        return v
    base = baseline[0]
    if base.job not in measured:
        v.failures.append(f"the baseline {base.job} gave the wrong answer")
        return v

    parallel = [
        r for r in rounds if _overlapping([evidence[c.job] for c in report.jobs if c.round == r])
    ]
    if not parallel:
        v.failures.append("no round ran two candidate Jobs at the same time")

    candidates = [c for c in report.jobs if c.round != 0 and c.job in measured]
    if not candidates:
        v.failures.append("no optimized Job gave the right answer")
        return v
    best = max(candidates, key=lambda c: _f(measured[c.job], "ops_per_s"))
    base_ops = _f(measured[base.job], "ops_per_s")
    best_ops = _f(measured[best.job], "ops_per_s")
    v.speedup = best_ops / base_ops
    v.baseline = {"job": base.job, "image": base.image, "ops_per_s": base_ops}
    v.best = {"job": best.job, "image": best.image, "ops_per_s": best_ops}

    if report.best_job != best.job:
        v.failures.append(f"the report calls {report.best_job} best; the evidence says {best.job}")
    if not _close(report.speedup, v.speedup):
        v.failures.append(f"the report claims {report.speedup:.2f}x; the evidence gives {v.speedup:.2f}x")

    b, w = evidence[base.job], evidence[best.job]
    claimed_saved = (_f(measured[base.job], "total_ms") - _f(measured[best.job], "total_ms")) / 1e3
    if b.wall_s is not None and w.wall_s is not None and claimed_saved > 0:
        wall_saved = b.wall_s - w.wall_s
        if wall_saved + WALL_SLACK_S < claimed_saved / 2:
            v.failures.append(
                f"{best.job} claims to save {claimed_saved:.1f}s of {base.job}'s time, "
                f"but its Job ran only {wall_saved:.1f}s shorter"
            )
    if v.speedup < p.min_speedup:
        v.failures.append(f"speedup {v.speedup:.2f}x is under the {p.min_speedup:g}x threshold")
    return v


def _f(result: dict[str, object], key: str) -> float:
    value = result.get(key)
    return float(value) if isinstance(value, (int, float)) and not isinstance(value, bool) else 0.0


def _expected_checksum(
    o: Observed, baseline: list[Claim], evidence: dict[str, Observed], p: Params
) -> str | None:
    if p.variant == "cpu":
        return reference_checksum(pod_seed(o.pod or ""), int(_f(o.result or {}, "records")))
    base = evidence.get(baseline[0].job) if baseline else None
    checksum = (base.result or {}).get("checksum") if base else None
    return checksum if isinstance(checksum, str) and checksum != "mismatch" else None


def _check_job(c: Claim, o: Observed | None, ctx: Context, registry: Registry) -> str | None:
    """Why the evidence does not back this claim, or None."""
    if o is None:
        return "crucible-kueue has no record of it"
    if o.run != ctx.run:
        return f"it belongs to run {o.run!r}, not this run {ctx.run!r}"
    if not o.succeeded:
        return "it did not succeed"
    r = o.result
    if r is None or o.pod is None:
        return "its log has no single result line"
    if r.get("pod") != o.pod:
        return f"its result names pod {r.get('pod')!r}, but it ran as {o.pod!r}"
    expected_records = ctx.records if ctx.params.variant == "cpu" else GPU_ROWS
    if r.get("records") != expected_records or r.get("iterations") != ITERATIONS:
        return f"it ran {r.get('records')} records x {r.get('iterations')}, not the fixed workload"
    if ctx.params.variant == "cpu" and r.get("seed") != str(pod_seed(o.pod)):
        return "its seed is not the one its pod name gives"
    if c.checksum != r.get("checksum"):
        return f"the report claims checksum {c.checksum}, its log says {r.get('checksum')}"
    for key, claimed in (("ops_per_s", c.ops_per_s), ("p50_ms", c.p50_ms)):
        if not _close(claimed, _f(r, key)):
            return f"the report claims {key} {claimed}, its log says {r.get(key)}"
    if o.wall_s is not None and _f(r, "total_ms") / 1e3 > o.wall_s + WALL_SLACK_S:
        return f"its log claims {_f(r, 'total_ms') / 1e3:.1f}s of work in a {o.wall_s:.1f}s Job"
    try:
        image = Image.parse(c.image, ctx.push_repo)
        wanted = BINARY.get(ctx.params.variant)
        hashed = image_files(registry, image, wanted or "")
    except Invalid as e:
        return str(e)
    if wanted is not None and hashed != r.get("exe_sha256"):
        return f"the binary it ran is not the /{wanted} in {c.image}"
    return None


def _overlapping(jobs: list[Observed]) -> bool:
    spans = sorted((o.started, o.completed) for o in jobs if o.started and o.completed)
    return any(spans[i + 1][0] < spans[i][1] for i in range(len(spans) - 1))


# --- entry points ----------------------------------------------------------------------------


def _load_json(path: Path) -> object:
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as e:
        raise Invalid(f"{path}: {e}") from e


def _inputs() -> dict[str, object]:
    path = os.environ.get("CRUCIBLE_INPUTS_FILE")
    raw = _load_json(Path(path)) if path else {}
    return raw if isinstance(raw, dict) else {}


def run(mode: str, workspace: Path, layout: Path | None) -> dict[str, object]:
    report = Report.parse(_load_json(workspace / "inputs/optimize/report.json"))
    if mode == "gate":
        return {"status": "pass", "jobs": len(report.jobs)}
    params_raw = _inputs().get("params")
    params = Params.parse(params_raw if isinstance(params_raw, dict) else {})
    harness = HARNESS[params.variant]
    submitted = workspace / "inputs/optimize" / harness
    tampered = not submitted.is_file() or submitted.read_bytes() != (workspace / harness).read_bytes()
    push_repo = push_repo_from_env(os.environ.get("CRUCIBLE_AGENT_SERVERS", ""))
    registry: Registry
    if layout is not None:
        registry = LayoutRegistry(layout)
    else:
        authfile_path = os.environ.get("FORGE_AUTHFILE")
        authfile = _load_json(Path(authfile_path)) if authfile_path else {}
        auth = basic_auth(authfile if isinstance(authfile, dict) else {}, push_repo)
        registry = HttpRegistry(push_repo, auth)
    evidence = parse_evidence(_load_json(workspace / "inputs/collect/evidence/kueue.json"))
    ctx = Context(
        params=params,
        push_repo=push_repo,
        run=os.environ.get("HOSTNAME") or socket.gethostname(),
        harness_tampered=tampered,
    )
    return verify(report, evidence, ctx, registry).line()


def main(argv: list[str]) -> int:
    if len(argv) < 2 or argv[1] not in ("gate", "verify"):
        print(__doc__, file=sys.stderr)
        return 2
    layout = Path(argv[3]) if len(argv) == 4 and argv[2] == "--oci-layout" else None
    try:
        line = run(argv[1], Path.cwd(), layout)
    except Invalid as e:
        line = {"pass": False, "status": "fail", "score": 0.0, "failures": [str(e)]}
    print(json.dumps(line))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
