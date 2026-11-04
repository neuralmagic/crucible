"""The informer's cache declarations and the bookkeeping it keeps between builds, against real git
trees and tar.

    python3 -m unittest discover -s examples/vllm-build-bench/bench
"""

from __future__ import annotations

import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import informer as t  # noqa: E402

BENCH = Path(__file__).resolve().parent
GIT = {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
# buildit's rules for a declared cache.
NAME = re.compile(r"^[a-z0-9][a-z0-9-]{0,62}$")
MAX_CACHES = 8
FACTS_MOUNT = "/buildit"


def git(cwd: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-c", "user.email=b@x", "-c", "user.name=b", "-c", "commit.gpgsign=false", *args],
        cwd=cwd, env={**os.environ, **GIT}, check=True, capture_output=True, text=True,
    ).stdout.strip()


def informer(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(BENCH / "informer.py"), *args], capture_output=True, text=True
    )


class TestInformer(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.tree = self.tmp / "vllm"
        files = {
            "csrc/layernorm_kernels.cu": "int leaf() { return 1; }\n",
            "csrc/dispatch_utils.h": "#pragma once\n",
            "CMakeLists.txt": "project(vllm)\n",
            "setup.py": "print('setup')\n",
            "vllm/_custom_ops.py": "OPS = []\n",
            "vllm/logger.py": "import logging\n",
        }
        for rel, text in files.items():
            (self.tree / rel).parent.mkdir(parents=True, exist_ok=True)
            (self.tree / rel).write_text(text)
        git(self.tree, "init", "-q")
        git(self.tree, "add", "-A")
        git(self.tree, "commit", "-qm", "base")

    def test_caches_are_what_buildit_accepts(self) -> None:
        out = informer("caches")
        self.assertEqual(out.returncode, 0, out.stderr)
        caches = json.loads(out.stdout)
        self.assertEqual(caches, [
            {"name": c.name, "path": str(c.path), "read_only": c.read_only} for c in t.CACHES
        ])
        self.assertLessEqual(len(caches), MAX_CACHES)
        self.assertEqual(len({c["name"] for c in caches}), len(caches))
        self.assertEqual(len({c["path"] for c in caches}), len(caches))
        for c in caches:
            with self.subTest(cache=c["name"]):
                self.assertRegex(c["name"], NAME)
                path = Path(c["path"])
                self.assertTrue(path.is_absolute())
                self.assertNotIn(Path(FACTS_MOUNT), [path, *path.parents])
                self.assertFalse(set(":,\0") & set(c["path"]))
                self.assertFalse({".", ".."} & set(path.parts))

    def test_the_manifest_covers_tracked_compiled_sources_only(self) -> None:
        (self.tree / "csrc/untracked.cu").write_text("int x;\n")
        got = t.manifest(self.tree)
        self.assertEqual(
            sorted(got), ["CMakeLists.txt", "csrc/dispatch_utils.h", "csrc/layernorm_kernels.cu",
                          "setup.py", "vllm/_custom_ops.py"],
        )
        self.assertEqual(len(set(got.values())), len(got))

    def test_sources_are_dated_by_content_not_by_how_they_arrived(self) -> None:
        last = t.manifest(self.tree)
        self.assertEqual(t.touch_changed(self.tree, {}, last), [], "no last build, nothing to touch")
        leaf = self.tree / "csrc/layernorm_kernels.cu"
        header = self.tree / "csrc/dispatch_utils.h"
        for f in (leaf, header):
            os.utime(f, (1_000_000, 1_000_000))
        leaf.write_text("int leaf() { return 2; }\n")
        os.utime(leaf, (1_000_000, 1_000_000))
        fresh = self.tree / "setup.py"
        fresh.touch()
        touched = t.touch_changed(self.tree, last, t.manifest(self.tree))
        self.assertEqual(touched, ["csrc/layernorm_kernels.cu"])
        self.assertGreater(leaf.stat().st_mtime, 1_000_000)
        for unchanged in (header, fresh):
            self.assertEqual(unchanged.stat().st_mtime, t.SETTLED_MTIME, unchanged)

    def test_generated_sources_come_back_without_overwriting_the_tree(self) -> None:
        gen = self.tree / "csrc/quantization/gptq_marlin/kernel_bf16.cu"
        gen.parent.mkdir(parents=True)
        gen.write_text("// generated\n")
        (self.tree / "csrc/kept.cu").write_text("// first\n")
        saved = self.tmp / "cmake" / "generated.tar"
        saved.parent.mkdir()
        t.save_generated(self.tree, saved)
        self.assertEqual(sorted(p.name for p in saved.parent.iterdir()), ["generated.tar"])
        gen.unlink()
        (self.tree / "csrc/kept.cu").write_text("// edited since\n")
        t.restore_generated(self.tree, saved)
        self.assertEqual(gen.read_text(), "// generated\n")
        self.assertEqual((self.tree / "csrc/kept.cu").read_text(), "// edited since\n")
        t.restore_generated(self.tree, self.tmp / "missing.tar")

    def test_stored_outputs_are_the_files_written_since_the_stamp_and_restore_whole(self) -> None:
        for f in (self.tree / "vllm").iterdir():
            os.utime(f, (1_000_000, 1_000_000))
        stamp = self.tmp / "stamp"
        stamp.touch()
        os.utime(stamp, (2_000_000, 2_000_000))
        (self.tree / "vllm/_C.abi3.so").write_bytes(b"\x7fELF built")
        (self.tree / "vllm/vllm_flash_attn").mkdir()
        (self.tree / "vllm/vllm_flash_attn/_vllm_fa3_C.abi3.so").write_bytes(b"\x7fELF fa3")
        (self.tree / "vllm/link.so").symlink_to("_C.abi3.so")
        outputs = t.written_since(self.tree, "vllm", stamp)
        self.assertEqual(outputs, ["vllm/_C.abi3.so", "vllm/vllm_flash_attn/_vllm_fa3_C.abi3.so"])
        stored = self.tmp / "store" / "k.tar"
        stored.parent.mkdir()
        t.tar_into(stored, self.tree, outputs)
        self.assertFalse(stored.with_name("k.tar.part").exists())
        (self.tree / "vllm/_C.abi3.so").write_bytes(b"stale")
        shutil.rmtree(self.tree / "vllm/vllm_flash_attn")
        t.untar(stored, self.tree, keep_existing=False)
        self.assertEqual((self.tree / "vllm/_C.abi3.so").read_bytes(), b"\x7fELF built")
        self.assertEqual((self.tree / "vllm/vllm_flash_attn/_vllm_fa3_C.abi3.so").read_bytes(), b"\x7fELF fa3")

    def test_a_derive_build_reports_its_facts_without_compiling(self) -> None:
        out = informer("build", str(self.tree), "--base", "img", "--mode", "derive")
        self.assertEqual(out.returncode, 0, out.stderr)
        line = out.stdout.strip().splitlines()[-1]
        self.assertTrue(line.startswith("informer-facts "), line)
        facts = json.loads(line.removeprefix("informer-facts "))
        self.assertEqual(facts.keys(), {"mode", "store", "shared", "build_s"})
        self.assertEqual((facts["mode"], facts["store"], facts["shared"]), ("derive", "off", "off"))
        self.assertEqual(git(self.tree, "status", "--porcelain"), "")

    def test_bad_arguments_exit_with_usage(self) -> None:
        self.assertEqual(informer().returncode, 2)
        self.assertEqual(informer("build", str(self.tree)).returncode, 2, "--base is required")
        self.assertEqual(informer("build", str(self.tree), "--base", "x", "--mode", "x").returncode, 2)


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


@unittest.skipUnless(os.environ.get("BUILDIT_BIN"), "BUILDIT_BIN names no buildit binary")
class TestSharedCache(unittest.TestCase):
    """The informer against a real `buildit cache`."""

    TOKEN = "0f3c9a7e5b1d4c2a"

    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        (self.tmp / "token").write_text(self.TOKEN + "\n")
        port = free_port()
        self.url = f"http://127.0.0.1:{port}"
        server = subprocess.Popen(
            [os.environ["BUILDIT_BIN"], "cache", "--bind", f"127.0.0.1:{port}",
             "--dir", str(self.tmp / "data"), "--token-file", str(self.tmp / "token")],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        self.addCleanup(server.wait)
        self.addCleanup(server.terminate)
        for _ in range(100):
            try:
                with urllib.request.urlopen(f"{self.url}/_health", timeout=1):
                    return
            except OSError:
                time.sleep(0.1)
        self.fail("buildit cache did not come up")

    def blobs(self) -> list[str]:
        root = self.tmp / "data" / "blobs"
        return sorted(p.relative_to(root).as_posix() for p in root.rglob("*") if p.is_file())

    def test_the_cache_doc_names_the_cache_and_maybe_a_token(self) -> None:
        doc = self.tmp / "cache.json"
        cases: list[tuple[str, t.Remote | None]] = [
            ('{"url": "http://c.svc:8850/", "token": "abc"}', t.Remote("http://c.svc:8850", "abc")),
            ('{"url": "http://c.svc:8850/"}', t.Remote("http://c.svc:8850", None)),
            ('{"url": "http://c.svc:8850", "token": ""}', t.Remote("http://c.svc:8850", None)),
            ('{"url": "https://c.svc"}', None),
            ('{"token": "abc"}', None),
            ("[]", None),
            ("not json", None),
        ]
        for text, want in cases:
            with self.subTest(text=text):
                doc.write_text(text)
                self.assertEqual(t.Remote.load(doc), want)
        self.assertIsNone(t.Remote.load(self.tmp / "missing.json"))

    def test_ccache_gets_the_token_as_a_password_or_reads_only(self) -> None:
        rw = t.Remote("http://c.svc:8850", "a/b+c")
        self.assertEqual(rw.ccache_storage(), "http://informer:a%2Fb%2Bc@c.svc:8850/ccache")
        ro = t.Remote("http://c.svc:8850", None)
        self.assertEqual(ro.ccache_storage(), "http://c.svc:8850/ccache|read-only")

    def test_stored_outputs_go_through_the_shared_store(self) -> None:
        tree = self.tmp / "tree"
        (tree / "vllm").mkdir(parents=True)
        (tree / "vllm/_C.abi3.so").write_bytes(b"\x7fELF outputs")
        local = self.tmp / "store"
        local.mkdir()
        stored = local / "k1.tar"
        t.tar_into(stored, tree, ["vllm/_C.abi3.so"])
        writer = t.Remote(self.url, self.TOKEN)
        reader = t.Remote(self.url, None)

        self.assertFalse(reader.fetch("k1.tar", self.tmp / "got.tar"))
        self.assertEqual(sorted(p.name for p in self.tmp.iterdir() if p.name.startswith("got")), [])
        self.assertEqual(reader.publish("k1.tar", stored), "read-only")
        wrong = t.Remote(self.url, "nope").publish("k1.tar", stored)
        self.assertTrue(wrong.startswith("failed:") and "401" in wrong, wrong)
        self.assertEqual(self.blobs(), [])
        self.assertEqual(writer.publish("k1.tar", stored), "stored")
        self.assertEqual(self.blobs(), ["store/k1.tar"])

        stored.unlink()
        (tree / "vllm/_C.abi3.so").write_bytes(b"stale")
        self.assertEqual(t.restore(tree, stored, reader), "remote-hit")
        self.assertEqual((tree / "vllm/_C.abi3.so").read_bytes(), b"\x7fELF outputs")
        self.assertTrue(stored.exists(), "a remote hit fills the local store")
        self.assertEqual(t.restore(tree, stored, None), "hit")
        self.assertIsNone(t.restore(tree, local / "k2.tar", reader))
        self.assertIsNone(t.restore(tree, local / "k2.tar", None))
        self.assertFalse((local / "k2.tar").exists() or (local / "k2.tar.part").exists())

    def test_an_unreachable_cache_is_a_miss_not_a_failure(self) -> None:
        gone = t.Remote(f"http://127.0.0.1:{free_port()}", self.TOKEN)
        self.assertFalse(gone.fetch("k.tar", self.tmp / "k.tar"))
        self.assertFalse((self.tmp / "k.tar.part").exists())
        src = self.tmp / "src.tar"
        src.write_bytes(b"x")
        self.assertTrue(gone.publish("k.tar", src).startswith("failed:"))

    def test_ccache_shares_objects_through_the_informers_urls(self) -> None:
        if not (shutil.which("ccache") and shutil.which("cc")):
            self.skipTest("no ccache or cc")
        src = self.tmp / "src"
        src.mkdir()
        (src / "a.c").write_text("int a(int x) { return x * 7; }\n")
        (src / "b.c").write_text("int b(int x) { return x * 11; }\n")

        def compile(local: str, remote: t.Remote, name: str) -> dict[str, object]:
            env = os.environ | {
                "CCACHE_DIR": str(self.tmp / local),
                "CCACHE_REMOTE_STORAGE": remote.ccache_storage(),
                "CCACHE_SECONDARY_STORAGE": remote.ccache_storage(),
            }
            run = lambda *a: subprocess.run(  # noqa: E731
                ["ccache", *a], cwd=src, env=env, check=True, capture_output=True, text=True
            ).stdout
            run("-z")
            run("cc", "-c", name, "-o", f"{name}.o")
            return t.ccache_counts(run("--print-stats"))

        first = compile("l1", t.Remote(self.url, self.TOKEN), "a.c")
        self.assertEqual((first["ccache_misses"], first["ccache_remote_hits"]), (1, 0), first)
        stored = self.blobs()
        self.assertTrue(stored and all(b.startswith("ccache/") for b in stored), stored)
        second = compile("l2", t.Remote(self.url, None), "a.c")
        self.assertEqual((second["ccache_hits"], second["ccache_remote_hits"]), (1, 1), second)
        compile("l3", t.Remote(self.url, None), "b.c")
        self.assertEqual(self.blobs(), stored, "a read-only build stores nothing")


if __name__ == "__main__":
    unittest.main()
