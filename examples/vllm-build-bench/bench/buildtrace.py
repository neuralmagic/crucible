"""A CMake/Ninja build as a Chrome trace: the build's own phases, each job ninja ran on wall-clock
time, what ccache did with each compile, and when the build printed each line, so the stretches
where it printed nothing show. Load the JSON at https://ui.perfetto.dev.

    timeline = Timeline()
    with timeline.span("compile"):
        ...  # run the build with CCACHE_STATSLOG=stats.log
    jobs = ninja_jobs(build_dir, since_ns=timeline.origin_ns)
    trace = chrome_trace(timeline, jobs, ccache_outcomes(Path("stats.log")), meta={})

`run_logged` runs the build itself, timing each line it prints for `chrome_trace(lines=...)`.
`html` renders a trace as one self-contained page; from the command line:

    python3 buildtrace.py html --ninja-log build/.ninja_log --ccache-stats stats.log -o trace.html
    python3 buildtrace.py html --trace build-trace.json -o trace.html
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import time
from collections.abc import Generator, Sequence
from contextlib import contextmanager
from dataclasses import dataclass, field
from pathlib import Path

NINJA_LOG = ".ninja_log"
LOG_VERSION = re.compile(r"^# ninja log v(\d+)")
START_STAMPED_FROM = 7
MS = 1_000_000
QUIET_NS = 1_000 * MS
RUN_GAP_NS = 5_000 * MS
LABEL_CHARS = 120
QUIETEST = 10
MAX_INSTANTS = 2000
OBJECT = re.compile(r"(?:^|/)CMakeFiles/[^/]+\.dir/(.+)\.(?:o|obj)$")
REMOTE_HITS = ("remote_storage_hit", "remote_storage_read_hit", "secondary_storage_hit")
LOCAL_HITS = ("direct_cache_hit", "preprocessed_cache_hit")
SHARED_LIBRARIES = {".so", ".dylib", ".dll"}


@dataclass(frozen=True)
class Job:
    output: str
    start_ns: int
    end_ns: int


@dataclass(frozen=True)
class Span:
    name: str
    start_ns: int
    end_ns: int
    args: dict[str, object]


@dataclass(frozen=True)
class Line:
    at_ns: int
    text: str


@dataclass
class Timeline:
    """Named phases of a build, on wall-clock time from ORIGIN_NS."""

    origin_ns: int = field(default_factory=time.time_ns)
    spans: list[Span] = field(default_factory=list)

    @contextmanager
    def span(self, name: str, **args: object) -> Generator[dict[str, object]]:
        """Time the block as phase NAME; what the block adds to the yielded dict is kept as args."""
        start = time.time_ns()
        extra: dict[str, object] = dict(args)
        try:
            yield extra
        finally:
            self.spans.append(Span(name, start, time.time_ns(), extra))

    def seconds(self) -> dict[str, float]:
        """Each phase's total seconds."""
        out: dict[str, float] = {}
        for s in self.spans:
            out[s.name] = round(out.get(s.name, 0.0) + (s.end_ns - s.start_ns) / 1e9, 2)
        return out


def run_logged(cmd: list[str], cwd: Path, env: dict[str, str], log: Path) -> tuple[int, list[Line]]:
    """Run CMD with its stdout and stderr written to LOG, and the time each line arrived. A Python
    child is unbuffered so its lines arrive as printed."""
    lines: list[Line] = []
    with (
        log.open("w") as out,
        subprocess.Popen(
            cmd,
            cwd=cwd,
            env=env | {"PYTHONUNBUFFERED": "1"},
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            errors="replace",
        ) as proc,
    ):
        assert proc.stdout is not None
        for text in proc.stdout:
            lines.append(Line(time.time_ns(), text.rstrip("\n")))
            out.write(text)
    return proc.returncode, lines


def quiet_gaps(lines: Sequence[Line], since_ns: int) -> list[tuple[int, int, str]]:
    """The stretches of at least QUIET_NS in which the build printed nothing, from SINCE_NS, as
    (start, end, the line printed just before)."""
    gaps: list[tuple[int, int, str]] = []
    last, label = since_ns, "(start)"
    for line in lines:
        if line.at_ns - last >= QUIET_NS:
            gaps.append((last, line.at_ns, label))
        last, label = line.at_ns, line.text.strip()[:LABEL_CHARS]
    return gaps


def shown_lines(lines: Sequence[Line], gaps: list[tuple[int, int, str]]) -> list[Line]:
    """At most MAX_INSTANTS of LINES to draw: those on either side of each quiet stretch first,
    then the rest in order."""
    if len(lines) <= MAX_INSTANTS:
        return list(lines)
    edges = {start for start, _, _ in gaps} | {end for _, end, _ in gaps}
    keep = [i for i, line in enumerate(lines) if line.at_ns in edges][:MAX_INSTANTS]
    kept = set(keep)
    rest = (i for i in range(len(lines)) if i not in kept)
    keep += [i for _, i in zip(range(MAX_INSTANTS - len(keep)), rest, strict=False)]
    return [lines[i] for i in sorted(keep)]


def ninja_jobs(build_dir: Path, since_ns: int) -> list[Job]:
    """The jobs ninja logged in BUILD_DIR with a stamp at or after SINCE_NS; see `ninja_log_jobs`."""
    return ninja_log_jobs(build_dir / NINJA_LOG, since_ns)


def ninja_log_jobs(log: Path, since_ns: int | None = None) -> list[Job]:
    """The jobs ninja logged in LOG with a stamp at or after SINCE_NS, on wall-clock time; without
    SINCE_NS, the jobs of the log's last ninja run, the lines whose run start lies within
    RUN_GAP_NS of the latest one.

    A log line gives a job's start and end in ms from its ninja run's start, and a stamp in ns:
    the output's mtime, near the job's end, in older logs; the job's start in newer ones. The
    run's start is the median of stamp - start or stamp - end, whichever is steadier across the
    jobs, or with too few jobs to tell, whichever the log's version (START_STAMPED_FROM) implies.
    Later lines for an output replace earlier ones, as in ninja itself.
    """
    try:
        lines = log.read_text(errors="replace").splitlines()
    except OSError:
        return []
    version = next((int(m[1]) for m in map(LOG_VERSION.match, lines[:1]) if m), 0)
    latest: dict[str, tuple[int, int, int]] = {}
    for line in lines:
        parts = line.split("\t")
        if line.startswith("#") or len(parts) < 4:
            continue
        try:
            start_ms, end_ms, mtime_ns = int(parts[0]), int(parts[1]), int(parts[2])
        except ValueError:
            continue
        latest[parts[3]] = (start_ms, end_ms, mtime_ns)
    if since_ns is None and latest:
        since_ns = _last_run_start(list(latest.values()), version) - RUN_GAP_NS
    rows = {out: r for out, r in latest.items() if r[2] >= (since_ns or 0)}
    if not rows:
        return []
    run_start = _run_start(list(rows.values()), version)
    return sorted(
        (Job(out, run_start + s * MS, run_start + e * MS) for out, (s, e, _) in rows.items()),
        key=lambda j: (j.start_ns, j.output),
    )


def _last_run_start(rows: list[tuple[int, int, int]], version: int) -> int:
    """The latest run's start: each line dated by its start (newer logs) or end (older ones)."""
    stamped_at_start = version >= START_STAMPED_FROM
    return max(stamp - (start if stamped_at_start else end) * MS for start, end, stamp in rows)


def _run_start(rows: list[tuple[int, int, int]], version: int) -> int:
    by_start = sorted(stamp - start * MS for start, _, stamp in rows)
    by_end = sorted(stamp - end * MS for _, end, stamp in rows)
    if len(rows) >= 3:
        offsets = by_start if _spread(by_start) <= _spread(by_end) else by_end
    else:
        offsets = by_start if version >= START_STAMPED_FROM else by_end
    return offsets[len(offsets) // 2]


def _spread(ordered: list[int]) -> int:
    return ordered[3 * len(ordered) // 4] - ordered[len(ordered) // 4]


def ccache_outcomes(stats_log: Path) -> list[tuple[str, str]]:
    """Each compile ccache logged to a CCACHE_STATSLOG file, in order, as (input, outcome):
    `remote-hit`, `hit`, `miss`, or `uncached` for a call ccache did not cache."""
    try:
        lines = stats_log.read_text(errors="replace").splitlines()
    except OSError:
        return []
    out: list[tuple[str, str]] = []
    source: str | None = None
    counters: set[str] = set()
    for line in [*lines, "# "]:
        if line.startswith("# "):
            if source:
                out.append((source, _outcome(counters)))
            source, counters = line[2:].strip(), set()
        elif line.strip():
            counters.add(line.strip())
    return out


def _outcome(counters: set[str]) -> str:
    if counters.intersection(REMOTE_HITS) and "local_storage_hit" not in counters:
        return "remote-hit"
    if counters.intersection(LOCAL_HITS):
        return "hit"
    if "cache_miss" in counters:
        return "miss"
    return "uncached"


def object_source(output: str) -> str | None:
    """The source a CMake object file was compiled from, as far as its path says:
    [<subproject>/]CMakeFiles/<target>.dir/<source>.o, where an out-of-tree source keeps its
    absolute path without the leading slash."""
    match = OBJECT.search(output)
    return match[1] if match else None


def job_kinds(jobs: list[Job], outcomes: list[tuple[str, str]]) -> dict[str, str]:
    """Output -> kind: a compile's ccache outcome (`unknown` when ccache logged none for it),
    `link` for a shared library, else `other`. Outcomes match compiles by source path, in order,
    so a source compiled for two targets takes two outcomes."""
    by_name: dict[str, list[tuple[str, str]]] = {}
    for source, outcome in outcomes:
        by_name.setdefault(Path(source).name, []).append((source, outcome))
    kinds: dict[str, str] = {}
    for job in jobs:
        source = object_source(job.output)
        if source is None:
            shared = SHARED_LIBRARIES.intersection(Path(job.output).suffixes)
            kinds[job.output] = "link" if shared else "other"
            continue
        kinds[job.output] = "unknown"
        candidates = by_name.get(Path(source).name, [])
        for i, (path, outcome) in enumerate(candidates):
            if path == source or path.endswith("/" + source):
                kinds[job.output] = outcome
                del candidates[i]
                break
    return kinds


def summary(jobs: list[Job], kinds: dict[str, str]) -> dict[str, object]:
    """Job counts and job-seconds by kind, and the wall-clock span ninja ran for."""
    count: dict[str, int] = {}
    busy: dict[str, float] = {}
    for job in jobs:
        kind = kinds.get(job.output, "other")
        count[kind] = count.get(kind, 0) + 1
        busy[kind] = round(busy.get(kind, 0.0) + (job.end_ns - job.start_ns) / 1e9, 1)
    wall = (max(j.end_ns for j in jobs) - min(j.start_ns for j in jobs)) / 1e9 if jobs else 0.0
    return {"jobs": count, "job_s": busy, "ninja_s": round(wall, 1)}


def chrome_trace(
    timeline: Timeline,
    jobs: list[Job],
    outcomes: list[tuple[str, str]],
    meta: dict[str, object],
    lines: Sequence[Line] = (),
    lines_since_ns: int | None = None,
) -> dict[str, object]:
    """The Chrome trace: phases in one process; ninja jobs in another, one thread per concurrent
    slot, each job's kind as its category; and the build's output lines as instants beside the
    quiet stretches between them, counted from LINES_SINCE_NS (default the timeline's origin).
    META goes in otherData, with phase seconds, the ninja summary and the quietest stretches."""
    kinds = job_kinds(jobs, outcomes)
    gaps = quiet_gaps(lines, timeline.origin_ns if lines_since_ns is None else lines_since_ns)

    def us(ns: int) -> float:
        return round((ns - timeline.origin_ns) / 1000, 1)

    events: list[dict[str, object]] = [
        {"ph": "M", "pid": 1, "tid": 0, "name": "process_name", "args": {"name": "build phases"}},
        {"ph": "M", "pid": 2, "tid": 0, "name": "process_name", "args": {"name": "ninja jobs"}},
        {"ph": "M", "pid": 3, "tid": 0, "name": "process_name", "args": {"name": "build output"}},
    ]
    for line in shown_lines(lines, gaps):
        events.append(
            {
                "name": line.text.strip()[:LABEL_CHARS],
                "cat": "line",
                "ph": "i",
                "s": "t",
                "pid": 3,
                "tid": 1,
                "ts": us(line.at_ns),
            }
        )
    for start, end, after in gaps:
        events.append(
            {
                "name": f"quiet after: {after}",
                "cat": "quiet",
                "ph": "X",
                "pid": 3,
                "tid": 2,
                "ts": us(start),
                "dur": us(end) - us(start),
                "args": {"after": after},
            }
        )
    quietest = sorted(gaps, key=lambda g: g[0] - g[1])[:QUIETEST]
    for s in timeline.spans:
        events.append(
            {
                "name": s.name,
                "cat": "phase",
                "ph": "X",
                "pid": 1,
                "tid": 1,
                "ts": us(s.start_ns),
                "dur": us(s.end_ns) - us(s.start_ns),
                "args": s.args,
            }
        )
    slots: list[int] = []
    for job in jobs:
        slot = next((i for i, free_at in enumerate(slots) if free_at <= job.start_ns), len(slots))
        if slot == len(slots):
            slots.append(job.end_ns)
        else:
            slots[slot] = job.end_ns
        kind = kinds[job.output]
        events.append(
            {
                "name": Path(job.output).name,
                "cat": kind,
                "ph": "X",
                "pid": 2,
                "tid": slot + 1,
                "ts": us(job.start_ns),
                "dur": us(job.end_ns) - us(job.start_ns),
                "args": {"output": job.output, "kind": kind},
            }
        )
    return {
        "traceEvents": events,
        "displayTimeUnit": "ms",
        "otherData": meta
        | {
            "phases_s": timeline.seconds(),
            "ninja": summary(jobs, kinds),
            "quiet": [{"after": a, "s": round((e - s) / 1e9, 1)} for s, e, a in quietest],
        },
    }


def html(trace: dict[str, object], title: str) -> str:
    """TRACE as one self-contained page: summary, zoomable timeline, compile time by target,
    slowest jobs and quiet stretches."""
    template = Path(__file__).with_name("trace.html").read_text()
    data = json.dumps(trace | {"otherData": {**trace.get("otherData", {}), "title": title}})
    return template.replace("__TITLE__", _escape(title)).replace(
        "/*TRACE*/null", data.replace("</", "<\\/")
    )


def _escape(text: str) -> str:
    return (
        text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;").replace('"', "&quot;")
    )


def logs_trace(
    ninja_log: Path, stats_log: Path | None, facts: dict[str, object]
) -> dict[str, object]:
    """The trace of a build known only by its ninja log and, optionally, ccache's stats log."""
    jobs = ninja_log_jobs(ninja_log)
    outcomes = ccache_outcomes(stats_log) if stats_log is not None else []
    origin = jobs[0].start_ns if jobs else 0
    return chrome_trace(Timeline(origin_ns=origin), jobs, outcomes, facts)


def main(argv: Sequence[str]) -> int:
    parser = argparse.ArgumentParser(prog="buildtrace.py")
    sub = parser.add_subparsers(dest="cmd", required=True)
    page = sub.add_parser("html", help="render a build trace as a self-contained html page")
    source = page.add_mutually_exclusive_group(required=True)
    source.add_argument("--trace", type=Path, help="a Chrome trace this module wrote")
    source.add_argument("--ninja-log", type=Path, help="a build dir's .ninja_log")
    page.add_argument("--ccache-stats", type=Path, help="ccache's stats_log for the same build")
    page.add_argument("--title", default="Build trace")
    page.add_argument("--fact", action="append", default=[], metavar="KEY=VALUE")
    page.add_argument("-o", "--out", type=Path, required=True)
    args = parser.parse_args(argv)
    facts: dict[str, object] = dict(kv.split("=", 1) for kv in args.fact if "=" in kv)
    if args.trace is not None:
        trace = json.loads(args.trace.read_text())
        trace["otherData"] = {**trace.get("otherData", {}), **facts}
    else:
        trace = logs_trace(args.ninja_log, args.ccache_stats, facts)
    args.out.write_text(html(trace, args.title))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
