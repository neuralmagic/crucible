"""buildtrace: ninja logs and ccache stats logs read into a Chrome trace, from hand-written logs and
from a real CMake/Ninja/ccache build where those tools are installed."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import buildtrace as t  # noqa: E402

MS = 1_000_000


def ninja_log(build_dir: Path, rows: list[tuple[int, int, int, str]], version: int = 7) -> None:
    lines = [f"# ninja log v{version}"] + [
        f"{s}\t{e}\t{m}\t{out}\tdeadbeef" for s, e, m, out in rows
    ]
    (build_dir / t.NINJA_LOG).write_text("\n".join(lines) + "\n")


class TestNinjaJobs(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)

    def test_an_mtime_stamped_log_dates_each_job_by_its_end(self) -> None:
        old_run, new_run = 1_000_000 * MS, 2_000_000 * MS
        ninja_log(
            self.tmp,
            [
                (0, 40, old_run + 40 * MS, "CMakeFiles/a.dir/a.c.o"),
                (0, 900, old_run + 900 * MS, "CMakeFiles/b.dir/b.c.o"),
                (5, 120, new_run + 120 * MS, "CMakeFiles/a.dir/a.c.o"),
                (6, 300, new_run + 300 * MS + 7 * MS, "CMakeFiles/c.dir/c.c.o"),
                (301, 350, new_run + 350 * MS, "libx.so"),
                (351, 400, new_run + 400 * MS, "liby.so"),
            ],
            version=5,
        )
        self.assertEqual(
            t.ninja_jobs(self.tmp, since_ns=new_run),
            [
                t.Job("CMakeFiles/a.dir/a.c.o", new_run + 5 * MS, new_run + 120 * MS),
                t.Job("CMakeFiles/c.dir/c.c.o", new_run + 6 * MS, new_run + 300 * MS),
                t.Job("libx.so", new_run + 301 * MS, new_run + 350 * MS),
                t.Job("liby.so", new_run + 351 * MS, new_run + 400 * MS),
            ],
            "the old run's b.c.o is not this build's; a.c.o's later line wins; the median "
            "offset ignores c.c.o's late mtime",
        )

    def test_a_start_stamped_log_dates_each_job_by_its_start(self) -> None:
        run = 3_000_000 * MS
        ninja_log(
            self.tmp,
            [
                (0, 688, run + 0 * MS, "CMakeFiles/t.dir/a.c.o"),
                (1, 707, run + 1 * MS, "CMakeFiles/t.dir/b.c.o"),
                (1, 719, run + 2 * MS, "CMakeFiles/t.dir/c.c.o"),
                (719, 819, run + 719 * MS, "libt.so"),
            ],
        )
        self.assertEqual(
            t.ninja_jobs(self.tmp, since_ns=run),
            [
                t.Job("CMakeFiles/t.dir/a.c.o", run, run + 688 * MS),
                t.Job("CMakeFiles/t.dir/b.c.o", run + 1 * MS, run + 707 * MS),
                t.Job("CMakeFiles/t.dir/c.c.o", run + 1 * MS, run + 719 * MS),
                t.Job("libt.so", run + 719 * MS, run + 819 * MS),
            ],
        )

    def test_with_too_few_jobs_to_tell_the_log_version_decides(self) -> None:
        run = 4_000_000 * MS
        row = [(10, 510, run + 10 * MS, "CMakeFiles/t.dir/a.c.o")]
        ninja_log(self.tmp, row, version=7)
        self.assertEqual(
            t.ninja_jobs(self.tmp, run), [t.Job(row[0][3], run + 10 * MS, run + 510 * MS)]
        )
        ninja_log(self.tmp, row, version=5)
        self.assertEqual(
            t.ninja_jobs(self.tmp, run), [t.Job(row[0][3], run - 490 * MS, run + 10 * MS)]
        )

    def test_without_a_start_a_log_reads_as_its_last_run(self) -> None:
        first, second = 1_000_000 * MS, 1_060_000 * MS
        ninja_log(
            self.tmp,
            [
                (0, 900, first, "CMakeFiles/_C.dir/a.cu.o"),
                (3, 800, first + 3 * MS, "CMakeFiles/_C.dir/b.cu.o"),
                (901, 950, first + 901 * MS, "_C.abi3.so"),
                (0, 400, second, "CMakeFiles/_C.dir/b.cu.o"),
                (401, 450, second + 401 * MS, "_C.abi3.so"),
            ],
        )
        self.assertEqual(
            t.ninja_log_jobs(self.tmp / t.NINJA_LOG),
            [
                t.Job("CMakeFiles/_C.dir/b.cu.o", second, second + 400 * MS),
                t.Job("_C.abi3.so", second + 401 * MS, second + 450 * MS),
            ],
            "an incremental build in a kept build dir: a.cu.o is the first run's",
        )
        self.assertEqual(len(t.ninja_log_jobs(self.tmp / t.NINJA_LOG, since_ns=0)), 3)

    def test_a_missing_empty_or_malformed_log_is_no_jobs(self) -> None:
        self.assertEqual(t.ninja_jobs(self.tmp, 0), [])
        (self.tmp / t.NINJA_LOG).write_text("# ninja log v7\nnot\ta\tlog\nx\ty\tz\tw\tv\n")
        self.assertEqual(t.ninja_jobs(self.tmp, 0), [])


class TestCcacheOutcomes(unittest.TestCase):
    def test_each_logged_compile_reads_as_remote_hit_hit_miss_or_uncached(self) -> None:
        log = Path(tempfile.mkdtemp()) / "stats.log"
        self.addCleanup(shutil.rmtree, log.parent)
        log.write_text(
            "# /src/csrc/a.cu\ndirect_cache_hit\nremote_storage_hit\nremote_storage_read_hit\n"
            "local_storage_miss\n"
            "# /src/csrc/b.cu\ndirect_cache_hit\nlocal_storage_hit\nlocal_storage_read_hit\n"
            "# /src/csrc/c.cu\ncache_miss\ndirect_cache_miss\nlocal_storage_write\n"
            "# /src/csrc/d.cu\ndirect_cache_hit\nlocal_storage_hit\nremote_storage_hit\n"
            "# /src/csrc/e.cpp\npreprocessed_cache_hit\nsecondary_storage_hit\n"
            "# conftest.c\ncalled_for_link\n"
        )
        self.assertEqual(
            t.ccache_outcomes(log),
            [
                ("/src/csrc/a.cu", "remote-hit"),
                ("/src/csrc/b.cu", "hit"),
                ("/src/csrc/c.cu", "miss"),
                ("/src/csrc/d.cu", "hit"),
                ("/src/csrc/e.cpp", "remote-hit"),
                ("conftest.c", "uncached"),
            ],
        )
        self.assertEqual(t.ccache_outcomes(log.parent / "absent.log"), [])


class TestKindsAndTrace(unittest.TestCase):
    def test_object_paths_name_their_sources_in_and_out_of_tree(self) -> None:
        self.assertEqual(
            t.object_source("CMakeFiles/_C.dir/csrc/layernorm_kernels.cu.o"),
            "csrc/layernorm_kernels.cu",
        )
        self.assertEqual(
            t.object_source("CMakeFiles/_fa2_C.dir/var/cache/deps/fa-src/csrc/flash_api.cpp.o"),
            "var/cache/deps/fa-src/csrc/flash_api.cpp",
        )
        self.assertEqual(
            t.object_source("_deps/flashmla-build/CMakeFiles/_flashmla_C.dir/csrc/sm90/mla.cu.o"),
            "csrc/sm90/mla.cu",
        )
        self.assertIsNone(t.object_source("_C.abi3.so"))
        self.assertIsNone(t.object_source("NotCMakeFiles/_C.dir/x.cpp.o"))
        self.assertIsNone(t.object_source("CMakeFiles/_C.dir/build.make"))

    def test_kinds_take_outcomes_by_source_in_order(self) -> None:
        jobs = [
            t.Job("CMakeFiles/_C.dir/csrc/torch_bindings.cpp.o", 0, 1),
            t.Job("CMakeFiles/_moe_C.dir/csrc/torch_bindings.cpp.o", 0, 1),
            t.Job("CMakeFiles/_fa2.dir/var/deps/fa/flash_api.cpp.o", 0, 1),
            t.Job("CMakeFiles/_C.dir/csrc/never_logged.cu.o", 0, 1),
            t.Job("_C.abi3.so", 0, 1),
            t.Job("generated/kernels.stamp", 0, 1),
        ]
        outcomes = [
            ("/src/vllm/csrc/torch_bindings.cpp", "miss"),
            ("/var/deps/fa/flash_api.cpp", "remote-hit"),
            ("/src/vllm/csrc/torch_bindings.cpp", "hit"),
            ("/elsewhere/other/flash_api.cpp", "miss"),
        ]
        self.assertEqual(
            t.job_kinds(jobs, outcomes),
            {
                "CMakeFiles/_C.dir/csrc/torch_bindings.cpp.o": "miss",
                "CMakeFiles/_moe_C.dir/csrc/torch_bindings.cpp.o": "hit",
                "CMakeFiles/_fa2.dir/var/deps/fa/flash_api.cpp.o": "remote-hit",
                "CMakeFiles/_C.dir/csrc/never_logged.cu.o": "unknown",
                "_C.abi3.so": "link",
                "generated/kernels.stamp": "other",
            },
        )

    def test_the_trace_puts_overlapping_jobs_on_separate_threads(self) -> None:
        timeline = t.Timeline(origin_ns=1_000 * MS)
        timeline.spans.append(t.Span("compile", 1_000 * MS, 1_400 * MS, {"touched": 2}))
        jobs = [
            t.Job("CMakeFiles/x.dir/a.c.o", 1_010 * MS, 1_100 * MS),
            t.Job("CMakeFiles/x.dir/b.c.o", 1_020 * MS, 1_050 * MS),
            t.Job("CMakeFiles/x.dir/c.c.o", 1_060 * MS, 1_200 * MS),
            t.Job("x.so", 1_200 * MS, 1_300 * MS),
        ]
        outcomes = [("/s/a.c", "miss"), ("/s/b.c", "hit"), ("/s/c.c", "remote-hit")]
        trace = t.chrome_trace(timeline, jobs, outcomes, {"facts": {"store": "miss"}})
        events = trace["traceEvents"]
        assert isinstance(events, list)
        spans = [e for e in events if e["ph"] == "X"]
        self.assertEqual(
            [(e["name"], e["pid"], e["tid"], e["cat"], e["ts"], e["dur"]) for e in spans],
            [
                ("compile", 1, 1, "phase", 0.0, 400_000.0),
                ("a.c.o", 2, 1, "miss", 10_000.0, 90_000.0),
                ("b.c.o", 2, 2, "hit", 20_000.0, 30_000.0),
                ("c.c.o", 2, 2, "remote-hit", 60_000.0, 140_000.0),
                ("x.so", 2, 1, "link", 200_000.0, 100_000.0),
            ],
            "b.c.o overlaps a.c.o so takes a second thread; c.c.o reuses it once free",
        )
        self.assertEqual(
            {e["args"]["name"] for e in events if e["ph"] == "M"},
            {"build phases", "ninja jobs", "build output"},
        )
        self.assertEqual(
            trace["otherData"],
            {
                "facts": {"store": "miss"},
                "phases_s": {"compile": 0.4},
                "ninja": {
                    "jobs": {"miss": 1, "hit": 1, "remote-hit": 1, "link": 1},
                    "job_s": {"miss": 0.1, "hit": 0.0, "remote-hit": 0.1, "link": 0.1},
                    "ninja_s": 0.3,
                },
                "quiet": [],
            },
        )

    def test_spans_time_their_blocks_and_keep_what_the_block_adds(self) -> None:
        timeline = t.Timeline()
        with timeline.span("fetch", key="k") as span:
            time.sleep(0.02)
            span["found"] = True
        with self.assertRaises(RuntimeError):
            with timeline.span("untar"):
                raise RuntimeError("boom")
        self.assertEqual(
            [(s.name, s.args) for s in timeline.spans],
            [("fetch", {"key": "k", "found": True}), ("untar", {})],
        )
        self.assertGreaterEqual(timeline.spans[0].end_ns - timeline.spans[0].start_ns, 20 * MS)
        self.assertEqual(set(timeline.seconds()), {"fetch", "untar"})


class TestBuildOutput(unittest.TestCase):
    def test_run_logged_times_each_line_as_it_is_printed_and_keeps_the_exit_status(self) -> None:
        tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp)
        log = tmp / "build.log"
        child = "import sys,time; print('configuring'); time.sleep(1.2); print('building'); "
        child += "sys.stderr.write('warning: x\\n'); sys.exit(3)"
        started = time.time_ns()
        code, lines = t.run_logged([sys.executable, "-c", child], tmp, dict(os.environ), log)
        self.assertEqual(code, 3)
        self.assertEqual([x.text for x in lines], ["configuring", "building", "warning: x"])
        self.assertEqual(log.read_text(), "configuring\nbuilding\nwarning: x\n")
        self.assertLess(lines[0].at_ns - started, 1_000 * MS, "the first line is not held back")
        self.assertGreaterEqual(lines[1].at_ns - lines[0].at_ns, 1_100 * MS)
        gaps = t.quiet_gaps(lines, started)
        self.assertEqual([(g[2]) for g in gaps], ["configuring"])
        self.assertEqual((gaps[0][0], gaps[0][1]), (lines[0].at_ns, lines[1].at_ns))

    def test_quiet_stretches_are_labelled_by_the_line_before_them(self) -> None:
        at = 10_000 * MS
        lines = [
            t.Line(at + 2_500 * MS, "-- Configuring done (2.0s)"),
            t.Line(at + 2_600 * MS, "[1/3] Building CXX object a.o"),
            t.Line(at + 3_500 * MS, "[2/3] Building CXX object b.o"),
            t.Line(at + 9_000 * MS, "  " + "x" * 200),
            t.Line(at + 9_100 * MS, "done"),
        ]
        self.assertEqual(
            t.quiet_gaps(lines, at),
            [
                (at, at + 2_500 * MS, "(start)"),
                (at + 3_500 * MS, at + 9_000 * MS, "[2/3] Building CXX object b.o"),
            ],
        )
        timeline = t.Timeline(origin_ns=at)
        trace = t.chrome_trace(timeline, [], [], {}, lines)
        events = trace["traceEvents"]
        assert isinstance(events, list)
        instants = [e for e in events if e["ph"] == "i"]
        self.assertEqual(len(instants), 5)
        self.assertEqual(instants[3]["name"], "x" * t.LABEL_CHARS)
        quiet = [(e["name"], e["ts"], e["dur"]) for e in events if e.get("cat") == "quiet"]
        self.assertEqual(
            quiet,
            [
                ("quiet after: (start)", 0.0, 2_500_000.0),
                ("quiet after: [2/3] Building CXX object b.o", 3_500_000.0, 5_500_000.0),
            ],
        )
        self.assertEqual(
            trace["otherData"]["quiet"],
            [{"after": "[2/3] Building CXX object b.o", "s": 5.5}, {"after": "(start)", "s": 2.5}],
            "quietest first",
        )

    def test_a_long_output_draws_the_quiet_edges_and_then_the_first_lines(self) -> None:
        at = 20_000 * MS
        lines = [t.Line(at + i * MS, f"[{i}/5000] job") for i in range(5000)]
        lines[4000] = t.Line(lines[3999].at_ns + 3_000 * MS, "after the pause")
        for i in range(4001, 5000):
            lines[i] = t.Line(lines[4000].at_ns + (i - 4000) * MS, lines[i].text)
        gaps = t.quiet_gaps(lines, at)
        shown = t.shown_lines(lines, gaps)
        self.assertEqual(len(shown), t.MAX_INSTANTS)
        self.assertEqual(shown, sorted(shown, key=lambda x: x.at_ns))
        self.assertIn(lines[3999], shown)
        self.assertIn(lines[4000], shown)
        self.assertEqual(shown[: t.MAX_INSTANTS - 2], lines[: t.MAX_INSTANTS - 2])
        self.assertEqual(t.shown_lines(lines[:10], []), lines[:10])


@unittest.skipUnless(
    all(shutil.which(x) for x in ("cmake", "ninja", "ccache", "cc")),
    "needs cmake, ninja, ccache, cc",
)
def embedded_trace(page: str) -> dict[str, object]:
    """The trace a rendered page carries, read back the way its script reads it."""
    start = page.index("const TRACE = ") + len("const TRACE = ")
    return json.loads(page[start:].split("\n", 1)[0].removesuffix(";"))


class TestHtml(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)

    def test_a_page_from_logs_carries_each_job_and_its_ccache_result(self) -> None:
        run = 5_000_000 * MS
        ninja_log(
            self.tmp,
            [
                (0, 900, run, "CMakeFiles/_C.dir/csrc/a.cu.o"),
                (2, 400, run + 2 * MS, "CMakeFiles/_C.dir/csrc/b.cu.o"),
                (901, 950, run + 901 * MS, "_C.abi3.so"),
            ],
        )
        stats = self.tmp / "stats.log"
        stats.write_text(
            "# /src/csrc/a.cu\ncache_miss\n# /src/csrc/b.cu\nremote_storage_read_hit\n"
        )
        out = self.tmp / "trace.html"

        self.assertEqual(
            t.main(
                [
                    "html",
                    "--ninja-log",
                    str(self.tmp / t.NINJA_LOG),
                    "--ccache-stats",
                    str(stats),
                    "--title",
                    "edit <1>",
                    "--fact",
                    "node=b200",
                    "-o",
                    str(out),
                ]
            ),
            0,
        )
        page = out.read_text()
        self.assertIn("<title>edit &lt;1&gt;</title>", page)
        trace = embedded_trace(page)
        jobs = {
            e["args"]["output"]: e["cat"]
            for e in trace["traceEvents"]
            if e.get("pid") == 2 and e.get("ph") == "X"
        }
        self.assertEqual(
            jobs,
            {
                "CMakeFiles/_C.dir/csrc/a.cu.o": "miss",
                "CMakeFiles/_C.dir/csrc/b.cu.o": "remote-hit",
                "_C.abi3.so": "link",
            },
        )
        self.assertEqual(trace["otherData"]["node"], "b200")
        self.assertEqual(trace["otherData"]["title"], "edit <1>")
        self.assertEqual(trace["otherData"]["ninja"]["ninja_s"], 0.9)

    def test_a_job_named_like_a_closing_script_tag_stays_inside_the_script(self) -> None:
        ninja_log(self.tmp, [(0, 10, 10 * MS, "CMakeFiles/x.dir/</script><b>.c.o")])
        page = t.html(t.logs_trace(self.tmp / t.NINJA_LOG, None, {}), "x")
        script = page[page.index("const TRACE = ") :]
        self.assertNotIn("</script><b>", script[: script.index("</script>\n</body>")])
        jobs = [e for e in embedded_trace(page)["traceEvents"] if e.get("ph") == "X"]
        self.assertEqual(jobs[0]["args"]["output"], "CMakeFiles/x.dir/</script><b>.c.o")

    def test_a_written_trace_renders_with_added_facts(self) -> None:
        timeline = t.Timeline(origin_ns=0)
        trace = t.chrome_trace(timeline, [t.Job("CMakeFiles/a.dir/a.c.o", 0, MS)], [], {"k": "v"})
        source = self.tmp / "trace.json"
        source.write_text(json.dumps(trace))
        out = self.tmp / "trace.html"
        t.main(["html", "--trace", str(source), "--fact", "run=7", "-o", str(out)])
        data = embedded_trace(out.read_text())
        self.assertEqual((data["otherData"]["k"], data["otherData"]["run"]), ("v", "7"))


class TestARealBuild(unittest.TestCase):
    """A CMake project compiled through ccache twice: once cold, once after its objects are gone
    so ccache serves them, with one source changed so it compiles."""

    def test_a_real_build_traces_misses_then_hits(self) -> None:
        root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, root)
        src, build = root / "src", root / "build"
        src.mkdir()
        (src / "CMakeLists.txt").write_text(
            "cmake_minimum_required(VERSION 3.20)\nproject(t C)\n"
            "add_library(t SHARED a.c b.c c.c)\n"
        )
        for name in "abc":
            (src / f"{name}.c").write_text(f"int {name}(int x) {{ return x + {ord(name)}; }}\n")
        stats = root / "stats.log"
        env = os.environ | {
            "CCACHE_DIR": str(root / "ccache"),
            "CCACHE_STATSLOG": str(stats),
            "CCACHE_NOHASHDIR": "true",
            "CCACHE_BASEDIR": str(root),
        }
        configure = [
            "cmake",
            "-G",
            "Ninja",
            "-S",
            str(src),
            "-B",
            str(build),
            "-DCMAKE_C_COMPILER_LAUNCHER=ccache",
        ]
        subprocess.run(configure, env=env, check=True, capture_output=True)

        def traced_build() -> tuple[list[t.Job], dict[str, str], t.Timeline]:
            stats.unlink(missing_ok=True)
            timeline = t.Timeline()
            with timeline.span("build"):
                subprocess.run(
                    ["ninja", "-C", str(build)], env=env, check=True, capture_output=True
                )
            jobs = t.ninja_jobs(build, timeline.origin_ns)
            return jobs, t.job_kinds(jobs, t.ccache_outcomes(stats)), timeline

        jobs, kinds, timeline = traced_build()
        objects = {j.output: kinds[j.output] for j in jobs if t.object_source(j.output)}
        self.assertEqual(sorted(objects.values()), ["miss", "miss", "miss"], kinds)
        self.assertEqual(sorted(k for k in kinds.values() if k == "link"), ["link"], kinds)
        window = timeline.spans[0]
        for job in jobs:
            self.assertLessEqual(window.start_ns - 5 * MS, job.start_ns, job)
            self.assertLessEqual(job.end_ns, window.end_ns + 5 * MS, job)

        for obj in build.rglob("*.o"):
            obj.unlink()
        (src / "c.c").write_text("int c(int x) { return x * 3; }\n")
        jobs, kinds, _ = traced_build()
        by_source = {
            t.object_source(j.output): kinds[j.output] for j in jobs if t.object_source(j.output)
        }
        self.assertEqual(
            {Path(s).name: k for s, k in by_source.items() if s},
            {"a.c": "hit", "b.c": "hit", "c.c": "miss"},
        )

        page = root / "trace.html"
        t.main(
            [
                "html",
                "--ninja-log",
                str(build / t.NINJA_LOG),
                "--ccache-stats",
                str(stats),
                "-o",
                str(page),
            ]
        )
        traced = {
            Path(e["args"]["output"]).name: e["cat"]
            for e in embedded_trace(page.read_text())["traceEvents"]
            if e.get("pid") == 2 and e.get("ph") == "X" and t.object_source(e["args"]["output"])
        }
        self.assertEqual(traced, {"a.c.o": "hit", "b.c.o": "hit", "c.c.o": "miss"})


if __name__ == "__main__":
    unittest.main()
