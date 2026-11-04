"""The replay against real git checkouts, a real C++ compiler, and the pack's own files.

    python3 -m unittest discover -s examples/vllm-build-bench/bench
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import replay as t  # noqa: E402

BENCH = Path(__file__).resolve().parent
PACK = BENCH.parent
GIT = {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}

HEADER = """/*
 * dispatch macros
 */
#pragma once

inline int crucible_twice(int x) { return 2 * x; }
"""
LEAF = """#include "dispatch_utils.h"

int crucible_leaf(int x) { return crucible_twice(x) + 1; }
"""
OTHER = """#include "dispatch_utils.h"

int crucible_other(int x) { return crucible_twice(x) - 1; }
"""
FILES = {
    "csrc/dispatch_utils.h": HEADER,
    "csrc/layernorm_kernels.cu": LEAF,
    "csrc/other.cu": OTHER,
    "cmake/utils.cmake": "function(noop)\nendfunction()\n",
    "CMakeLists.txt": "cmake_minimum_required(VERSION 3.26)\nproject(vllm)",
    "pyproject.toml": "[build-system]\nrequires = []\n",
    "setup.py": "print('setup')\n",
    "vllm/_custom_ops.py": "OPS = []\n",
    "vllm/logger.py": "import logging\n",
}


def git(cwd: Path, *args: str) -> str:
    env = {**os.environ, **GIT}
    return subprocess.run(
        ["git", "-c", "user.email=b@x", "-c", "user.name=b", "-c", "commit.gpgsign=false", *args],
        cwd=cwd, env=env, check=True, capture_output=True, text=True,
    ).stdout.strip()


def origin(root: Path, files: dict[str, str]) -> tuple[str, str]:
    """A vLLM-shaped repository; returns its file:// url and HEAD."""
    for rel, text in files.items():
        (root / rel).parent.mkdir(parents=True, exist_ok=True)
        (root / rel).write_text(text)
    git(root, "init", "-q")
    git(root, "config", "uploadpack.allowAnySHA1InWant", "true")
    git(root, "add", "-A")
    git(root, "commit", "-qm", "base")
    return root.as_uri(), git(root, "rev-parse", "HEAD")


def compiler() -> str | None:
    for cxx in ("g++", "c++", "clang++"):
        if shutil.which(cxx):
            return cxx
    return None


PARAMS = {
    "base_image": "docker.io/vllm/vllm-openai:v0.32.0@sha256:" + "b" * 64,
    "vllm_ref": "5df63a7203cdb9328d824a27b682e77c7b9928be",
    "arch_list": "10.0",
    "max_jobs": 16,
    "push": True,
    "timeout_s": 3600,
}


def record(step: str, candidate: float) -> dict[str, object]:
    return {
        "step": step,
        "mode": "full",
        "wall_s": candidate + 100,
        "candidate_s": candidate,
        "compile_s": candidate - 10,
        "push_s": 5.0,
    }


class TestTrace(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self._env = {k: os.environ.get(k) for k in GIT}
        os.environ.update(GIT)
        self.addCleanup(self._restore_env)

    def _restore_env(self) -> None:
        for k, v in self._env.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v

    def checkout(self) -> tuple[Path, str]:
        (self.tmp / "origin").mkdir()
        url, ref = origin(self.tmp / "origin", FILES)
        (self.tmp / "ws").mkdir()
        tree = t.checkout(self.tmp / "ws", ref, url)
        t.stage_bench(tree, BENCH)
        return tree, ref

    def test_each_step_resolves_its_mode_and_the_reset_restores_the_clean_tree(self) -> None:
        tree, _ = self.checkout()
        clean = t.compiled_digest(tree)
        seen: dict[str, tuple[str, str]] = {}
        for name, edits in t.STEPS.items():
            seen[name] = t.resolve(tree, edits)
            t.reset(tree)
            self.assertEqual(t.compiled_digest(tree), clean, name)
            self.assertEqual(git(tree, "status", "--porcelain"), "", name)
            self.assertTrue((tree / t.CONTAINERFILE).is_file(), name)
        self.assertEqual(seen["clean"], ("derive", clean))
        self.assertEqual(seen["revert"], ("derive", clean))
        for name in ("leaf", "leaf_py", "leaf_again", "header", "cmake"):
            self.assertEqual(seen[name][0], "full", name)
        self.assertEqual(seen["leaf_py"], seen["leaf"])
        self.assertEqual(seen["leaf_again"], seen["leaf"])
        digests = {seen[n][1] for n in ("clean", "leaf", "header", "cmake")}
        self.assertEqual(len(digests), 4)

    def test_a_python_edit_leaves_the_compiled_digest_and_a_csrc_edit_moves_it(self) -> None:
        tree, _ = self.checkout()
        clean = t.compiled_digest(tree)
        t.EDITS["py"].apply(tree)
        self.assertEqual(t.compiled_digest(tree), clean)
        (tree / "csrc/other.cu").write_text(OTHER + "// touched\n")
        self.assertNotEqual(t.compiled_digest(tree), clean)
        (tree / "csrc/other.cu").write_text(OTHER)
        (tree / "rust").mkdir()
        (tree / "rust/lib.rs").write_text("")
        self.assertNotEqual(t.compiled_digest(tree), clean)

    def test_checkout_keeps_a_matching_tree_and_replaces_anything_else(self) -> None:
        tree, ref = self.checkout()
        url = (self.tmp / "origin").as_uri()
        (tree / ".git" / "kept").write_text("")
        self.assertEqual(t.checkout(self.tmp / "ws", ref, url), tree)
        self.assertTrue((tree / ".git" / "kept").exists())
        shutil.rmtree(tree / ".git")
        t.checkout(self.tmp / "ws", ref, url)
        self.assertEqual(t.head(tree), ref)
        (self.tmp / "origin" / "csrc/other.cu").write_text(OTHER + "// next\n")
        git(self.tmp / "origin", "commit", "-qam", "next")
        nxt = git(self.tmp / "origin", "rev-parse", "HEAD")
        t.checkout(self.tmp / "ws", nxt, url)
        self.assertEqual(t.head(tree), nxt)
        self.assertFalse((tree / ".git" / "kept").exists())

    def test_a_missing_ref_fails(self) -> None:
        self.checkout()
        url = (self.tmp / "origin").as_uri()
        with self.assertRaises(t.Fail):
            t.checkout(self.tmp / "ws", "0" * 40, url)

    def test_markers_compile_without_warnings_and_survive_linking(self) -> None:
        cxx = compiler()
        if cxx is None:
            self.skipTest("no C++ compiler")
        tree, _ = self.checkout()
        t.EDITS["leaf"].apply(tree)
        t.EDITS["header"].apply(tree)
        header = (tree / "csrc/dispatch_utils.h").read_text().splitlines()
        self.assertEqual(header[3], "#pragma once")
        self.assertIn(t.MARKER + "header", header[4])
        lib = self.tmp / "lib.so"
        subprocess.run(
            [cxx, "-std=c++17", "-O2", "-Wall", "-Wextra", "-Werror", "-fvisibility=hidden",
             "-fPIC", "-shared", "-x", "c++", "csrc/layernorm_kernels.cu", "csrc/other.cu",
             "-o", str(lib)],
            cwd=tree, check=True, capture_output=True, text=True,
        )
        found = subprocess.run(
            ["grep", "-aoh", "crucible-bench-marker:[a-z_]*", str(lib)],
            check=True, capture_output=True, text=True,
        ).stdout.split()
        self.assertEqual(sorted(set(found)), [t.MARKER + "header", t.MARKER + "leaf"])

    def test_comment_markers_append_on_a_line_of_their_own(self) -> None:
        tree, _ = self.checkout()
        t.EDITS["cmake"].apply(tree)
        lines = (tree / "CMakeLists.txt").read_text().splitlines()
        self.assertEqual(lines[-2], "project(vllm)")
        self.assertEqual(lines[-1], f"# {t.MARKER}cmake")

    def test_the_grepped_log_yields_times_and_markers_but_not_the_echoed_run_line(self) -> None:
        log = [
            "STEP 7/9: RUN set -eu; echo \"crucible-bench-times $(tr '\\n' ' ' < x)\"; "
            "echo \"crucible-bench-found $found\"",
            "crucible-bench-times base_start=100 arch=x86_64 cpus=96 base_end=160 compile_start=200 "
            "compile_end=900 ",
            'informer-facts {"mode":"full","store":"miss"}',
            "crucible-bench-found crucible-bench-marker:leaf crucible-bench-marker:py ",
            "COMMIT buildit-abc",
            *[f"Copying blob sha256:{i:064x}" for i in range(60)],
        ]
        grep = re.compile(t.LOG_GREP)
        page = "\n".join(f"{i + 1}:{line}" for i, line in enumerate(log) if grep.search(line))
        self.assertEqual(page.count("\n"), 1, page)
        times, found, host = t.parse_log(page)
        self.assertEqual(times, {"base_start": 100, "base_end": 160, "compile_start": 200, "compile_end": 900})
        self.assertEqual(found, ["leaf", "py"])
        self.assertEqual(host, {"arch": "x86_64", "cpus": 96})
        self.assertTrue(grep.search("crucible-bench-found "))
        self.assertEqual(t.parse_log("7:crucible-bench-found \n")[1], [])
        self.assertEqual(t.parse_log("7:crucible-bench-found\n")[1], [])
        self.assertEqual(t.parse_log("1:crucible-bench-times base_start=1\n")[:2], (None, None))

    def test_segments_split_the_wall_time(self) -> None:
        times = {"base_start": 100, "base_end": 160, "compile_start": 200, "compile_end": 900}
        seg = t.segments(times, 40.0, 950.5)
        self.assertEqual(seg, {
            "base_cached": False, "before_base_s": 60.0, "base_s": 60.0, "copy_s": 40.0,
            "setup_s": 100.0, "compile_s": 700.0, "after_compile_s": 50.5, "candidate_s": 850.5,
        })
        parts = ("before_base_s", "base_s", "copy_s", "compile_s", "after_compile_s")
        self.assertAlmostEqual(sum(float(str(seg[k])) for k in parts), 950.5 - 40.0)

    def test_a_cached_base_step_takes_no_time_and_setup_covers_the_rest(self) -> None:
        earlier = {"base_start": 100, "base_end": 160}
        times = earlier | {"compile_start": 5_050, "compile_end": 5_200}
        seg = t.segments(times, 5_000.0, 5_230.0)
        self.assertEqual(seg, {
            "base_cached": True, "before_base_s": None, "base_s": 0.0, "copy_s": None,
            "setup_s": 50.0, "compile_s": 150.0, "after_compile_s": 30.0, "candidate_s": 230.0,
        })
        skewed = t.segments(earlier | {"compile_start": 300, "compile_end": 400}, 150.0, 450.0)
        self.assertFalse(skewed["base_cached"], "a base step within clock skew of the build ran in it")

    def test_the_staged_bench_is_what_the_containerfile_runs(self) -> None:
        tree, _ = self.checkout()
        staged = sorted(p.name for p in (tree / t.STAGED).iterdir())
        self.assertEqual(staged, sorted(t.BENCH_FILES))
        text = (BENCH / "Containerfile").read_text()
        self.assertIn(f"python3 /src/vllm/{t.STAGED}/{t.INFORMER} build /src/vllm", text)
        self.assertNotIn("--mount", text, "caches are declared to buildit, not mounted")
        packed = (PACK / "crucible.toml").read_text()
        for name in ("replay.py", *t.BENCH_FILES):
            self.assertIn(f'"bench/{name}"', packed)

    def test_the_informers_caches_become_buildit_cache_flags(self) -> None:
        flags = t.cache_args(BENCH)
        self.assertEqual(flags[0::2], ["--cache"] * 4)
        self.assertEqual(
            flags[1::2],
            [
                "vllm-ccache=/root/.cache/ccache",
                "vllm-builds=/var/cache/vllm-builds",
                "vllm-cmake=/var/cache/vllm-cmake",
                "vllm-envs=/var/cache/vllm-envs",
            ],
        )
        broken = self.tmp / "broken"
        broken.mkdir()
        (broken / t.INFORMER).write_text("print('[{\"name\": 1}]')\n")
        with self.assertRaisesRegex(t.Fail, "bad cache"):
            t.cache_args(broken)
        (broken / t.INFORMER).write_text("raise SystemExit(3)\n")
        with self.assertRaisesRegex(t.Fail, "exited 3"):
            t.cache_args(broken)

    def test_params_are_checked(self) -> None:
        p = t.Params.parse(PARAMS)
        self.assertEqual(p.max_jobs, 16)
        for key, bad in [
            ("vllm_ref", "v0.32.0"),
            ("max_jobs", True),
            ("max_jobs", 0),
            ("push", 1),
            ("base_image", ""),
        ]:
            with self.subTest(key=key, bad=bad), self.assertRaises(t.Fail):
                t.Params.parse({**PARAMS, key: bad})

    def test_build_args_are_the_containerfiles_args(self) -> None:
        text = (BENCH / "Containerfile").read_text()
        declared = set(re.findall(r"^ARG (\w+)", text, re.MULTILINE))
        args = t.Params.parse(PARAMS).build_args("full", "n0")
        passed = {a.split("=", 1)[0] for a in args[1::2]}
        self.assertEqual(passed, declared - {"VLLM_ENVS_REF"})
        self.assertIn("BUILD_NONCE=n0", args)
        self.assertNotIn("TORCH_CUDA_ARCH_LIST", declared)
        self.assertNotIn("MAX_JOBS", declared)
        self.assertIn("MARKER_FILES=CMakeLists.txt vllm/logger.py", args)

    def test_the_workflow_runs_the_trace_in_order(self) -> None:
        text = (PACK / "workflow.star").read_text()
        steps = re.search(r"^STEPS = \[(.*)\]$", text, re.MULTILINE)
        assert steps is not None
        self.assertEqual(re.findall(r'"(\w+)"', steps.group(1)), list(t.STEPS))
        defaults = re.search(r'"vllm_ref": \{[^}]*"default": "([0-9a-f]+)"', text)
        assert defaults is not None
        self.assertTrue(t.SHA.match(defaults.group(1)))

    def test_report_writes_every_step_and_sums_the_candidate_time(self) -> None:
        inputs: dict[str, object] = {"params": PARAMS}
        inputs |= {name: record(name, 100.0 * (i + 1)) for i, name in enumerate(t.STEPS)}
        out = t.report(inputs, self.tmp)
        self.assertEqual(out["candidate_s"], 2800.0)
        steps = out["steps"]
        assert isinstance(steps, list)
        self.assertEqual([s["step"] for s in steps], list(t.STEPS))
        written = json.loads((self.tmp / "report.json").read_text())
        self.assertEqual(written["path"], "dockerfile")
        self.assertEqual(written["params"]["vllm_ref"], PARAMS["vllm_ref"])
        del inputs["header"]
        with self.assertRaisesRegex(t.Fail, "header"):
            t.report(inputs, self.tmp)

    def test_the_cli_reports_and_fails_a_step_without_buildit(self) -> None:
        inputs = {"params": PARAMS} | {n: record(n, 10.0) for n in t.STEPS}
        (self.tmp / "inputs.json").write_text(json.dumps(inputs))
        env = {k: v for k, v in os.environ.items() if k != "CRUCIBLE_BUILDIT_SOCKET"}
        env["CRUCIBLE_INPUTS_FILE"] = str(self.tmp / "inputs.json")
        run = lambda *a: subprocess.run(  # noqa: E731
            [sys.executable, str(BENCH / "replay.py"), *a],
            cwd=self.tmp, env=env, capture_output=True, text=True,
        )
        ok = run("report")
        self.assertEqual(ok.returncode, 0, ok.stderr)
        self.assertEqual(json.loads(ok.stdout.splitlines()[-1])["candidate_s"], 70.0)
        failed = run("step", "leaf")
        self.assertEqual(failed.returncode, 1)
        self.assertIn("CRUCIBLE_BUILDIT_SOCKET", json.loads(failed.stdout.splitlines()[-1])["error"])
        self.assertFalse((self.tmp / t.TREE).exists())
        self.assertEqual(run("step", "nope").returncode, 1)
        self.assertEqual(run().returncode, 2)


if __name__ == "__main__":
    unittest.main()
