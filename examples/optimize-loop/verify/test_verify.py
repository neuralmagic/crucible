"""The verifier against real report, evidence and image-layout files.

    python3 -m unittest discover -s examples/optimize-loop/verify
"""

from __future__ import annotations

import gzip
import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import verify as v  # noqa: E402

PACK = Path(__file__).resolve().parent.parent
REPO = "registry.example.com/team/optimize"
RUN = "run-optimize-1"
RECORDS = 3_000
T0 = datetime(2026, 10, 24, 12, 0, 0, tzinfo=timezone.utc)


def stamp(at: datetime) -> str:
    return at.strftime("%Y-%m-%dT%H:%M:%SZ")


class Layout:
    """A real OCI image layout: one image per binary, one gzip layer holding /bench."""

    def __init__(self, root: Path) -> None:
        self.root = root
        (root / "blobs" / "sha256").mkdir(parents=True)
        (root / "oci-layout").write_text('{"imageLayoutVersion": "1.0.0"}')

    def _put(self, data: bytes) -> str:
        digest = hashlib.sha256(data).hexdigest()
        (self.root / "blobs" / "sha256" / digest).write_bytes(data)
        return "sha256:" + digest

    def image(self, files: dict[str, bytes]) -> str:
        raw = io.BytesIO()
        with tarfile.open(fileobj=raw, mode="w") as tar:
            for name, data in files.items():
                info = tarfile.TarInfo(name)
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
        layer = gzip.compress(raw.getvalue(), mtime=0)
        layer_digest = self._put(layer)
        config_digest = self._put(b'{"architecture": "amd64", "os": "linux"}')
        manifest = {
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest,
                "size": 40,
            },
            "layers": [
                {
                    "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                    "digest": layer_digest,
                    "size": len(layer),
                }
            ],
        }
        return self._put(json.dumps(manifest).encode())

    def index(self, manifest_digest: str) -> str:
        index = {
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": manifest_digest,
                    "size": 0,
                    "platform": {"os": "linux", "architecture": "amd64"},
                }
            ],
        }
        return self._put(json.dumps(index).encode())


class Run:
    """One consistent run: what each Job printed, what crucible-kueue says, and the report."""

    def __init__(self, layout: Layout, records: int = RECORDS) -> None:
        self.layout = layout
        self.records = records
        self.jobs: list[dict[str, object]] = []
        self.evidence: dict[str, object] = {"list": {"workloads": []}, "jobs": {}}

    def job(
        self,
        round: int,
        name: str,
        p50_ms: float,
        start_s: float,
        wall_s: float,
        binary: bytes | None = None,
    ) -> dict[str, object]:
        binary = binary or name.encode()
        digest = self.layout.image({"bench": binary})
        pod = f"{name}-x7k2p"
        result = {
            "variant": name,
            "pod": pod,
            "seed": str(v.pod_seed(pod)),
            "records": self.records,
            "iterations": v.ITERATIONS,
            "p50_ms": p50_ms,
            "ops_per_s": self.records / (p50_ms / 1e3),
            "total_ms": p50_ms * v.ITERATIONS + 50,
            "checksum": v.reference_checksum(v.pod_seed(pod), self.records),
            "exe_sha256": hashlib.sha256(binary).hexdigest(),
        }
        started = T0 + timedelta(seconds=start_s)
        self.evidence["list"]["workloads"].append(  # type: ignore[index]
            {"kind": "Job", "name": name, "run": RUN, "managed": True, "phase": "Finished"}
        )
        self.evidence["jobs"][name] = {  # type: ignore[index]
            "status": {
                "kind": "Job",
                "name": name,
                "phase": "Finished",
                "object_status": {
                    "startTime": stamp(started),
                    "completionTime": stamp(started + timedelta(seconds=wall_s)),
                    "succeeded": 1,
                },
            },
            "logs": {
                "pods_total": 1,
                "pods_shown": 1,
                "logs": [
                    {
                        "pod": pod,
                        "role": "pod",
                        "phase": "Succeeded",
                        "log": "optimize-loop-result " + json.dumps(result) + "\n",
                    }
                ],
            },
        }
        claim = {
            "round": round,
            "job": name,
            "image": f"{REPO}@{digest}",
            "checksum": result["checksum"],
            "ops_per_s": result["ops_per_s"],
            "p50_ms": p50_ms,
        }
        self.jobs.append(claim)
        return claim

    def result(self, name: str) -> dict[str, object]:
        log = self.evidence["jobs"][name]["logs"]["logs"][0]["log"]  # type: ignore[index]
        return json.loads(log.removeprefix("optimize-loop-result "))

    def set_result(self, name: str, result: dict[str, object]) -> None:
        self.evidence["jobs"][name]["logs"]["logs"][0]["log"] = (  # type: ignore[index]
            "optimize-loop-result " + json.dumps(result) + "\n"
        )

    def report(self, best: str, speedup: float) -> dict[str, object]:
        return {"jobs": self.jobs, "best_job": best, "speedup": speedup}


def standard(layout: Layout, records: int = RECORDS) -> tuple[Run, dict[str, object]]:
    """A baseline, a round of two parallel variants, then a round of one."""
    r = Run(layout, records)
    r.job(0, "ol-base", p50_ms=2000, start_s=0, wall_s=14)
    r.job(1, "ol-r1-map", p50_ms=40, start_s=30, wall_s=4)
    r.job(1, "ol-r1-sort", p50_ms=500, start_s=31, wall_s=6)
    r.job(2, "ol-r2-prealloc", p50_ms=20, start_s=60, wall_s=3)
    return r, r.report("ol-r2-prealloc", 100.0)


def check(
    r: Run,
    report: dict[str, object],
    layout: Layout,
    params: dict[str, object] | None = None,
    tampered: bool = False,
) -> v.Verdict:
    ctx = v.Context(
        params=v.Params.parse(params or {"gpu": False, "rounds": 3, "min_speedup": 5}),
        push_repo=REPO,
        run=RUN,
        records=RECORDS,
        harness_tampered=tampered,
    )
    return v.verify(
        v.Report.parse(report), v.parse_evidence(r.evidence), ctx, v.LayoutRegistry(layout.root)
    )


class VerifierTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = Path(tempfile.mkdtemp())
        self.layout = Layout(self.dir / "oci")

    def tearDown(self) -> None:
        shutil.rmtree(self.dir)

    def assertFails(self, verdict: v.Verdict, needle: str) -> None:
        self.assertFalse(verdict.passed, verdict.line())
        self.assertTrue(
            any(needle in f for f in verdict.failures),
            f"no failure mentions {needle!r}: {verdict.failures}",
        )

    def test_an_honest_run_passes_with_the_speedup_the_verifier_computes(self) -> None:
        r, report = standard(self.layout)
        verdict = check(r, report, self.layout)
        self.assertTrue(verdict.passed, verdict.failures)
        self.assertAlmostEqual(verdict.speedup or 0, 100.0)
        self.assertEqual((verdict.best or {})["job"], "ol-r2-prealloc")
        self.assertEqual((verdict.baseline or {})["job"], "ol-base")
        line = verdict.line()
        self.assertIs(line["pass"], True)
        self.assertAlmostEqual(float(line["score"]), 100.0)  # type: ignore[arg-type]
        rounds = line["rounds"]
        assert isinstance(rounds, list)
        self.assertEqual([x["round"] for x in rounds], [0, 1, 2])
        self.assertEqual([j["job"] for j in rounds[1]["jobs"]], ["ol-r1-map", "ol-r1-sort"])
        self.assertEqual(rounds[1]["best_ops_per_s"], r.result("ol-r1-map")["ops_per_s"])
        self.assertEqual(rounds[0]["jobs"][0]["p50_ms"], 2000)
        self.assertEqual(json.loads(json.dumps(line)), line, "the verdict is one JSON line")

    def test_a_failing_verdict_still_carries_the_rounds_it_measured(self) -> None:
        r, report = standard(self.layout)
        wrong = r.result("ol-r2-prealloc") | {"checksum": "f" * 64}
        r.set_result("ol-r2-prealloc", wrong)
        r.jobs[3]["checksum"] = "f" * 64
        rounds = check(r, report, self.layout).line()["rounds"]
        assert isinstance(rounds, list)
        self.assertEqual(rounds[2]["jobs"], [{"job": "ol-r2-prealloc", "rejected": "wrong answer"}])
        self.assertIsNone(rounds[2]["best_ops_per_s"])
        self.assertEqual(rounds[1]["best_ops_per_s"], r.result("ol-r1-map")["ops_per_s"])

    def test_a_doctored_checksum_is_refused(self) -> None:
        r, report = standard(self.layout)
        r.jobs[2]["checksum"] = "0" * 64
        self.assertFails(check(r, report, self.layout), "claims checksum")

    def test_a_job_that_printed_the_wrong_answer_is_not_credited(self) -> None:
        r, report = standard(self.layout)
        wrong = r.result("ol-r2-prealloc") | {"checksum": "f" * 64}
        r.set_result("ol-r2-prealloc", wrong)
        r.jobs[3]["checksum"] = "f" * 64
        verdict = check(r, report, self.layout)
        self.assertEqual(verdict.rejected, {"ol-r2-prealloc": "wrong answer"})
        self.assertFails(verdict, "the evidence says ol-r1-map")

    def test_a_wrong_answer_cannot_be_the_baseline(self) -> None:
        r, report = standard(self.layout)
        r.set_result("ol-base", r.result("ol-base") | {"checksum": "e" * 64})
        r.jobs[0]["checksum"] = "e" * 64
        self.assertFails(check(r, report, self.layout), "baseline ol-base gave the wrong answer")

    def test_an_image_by_tag_is_refused(self) -> None:
        r, report = standard(self.layout)
        r.jobs[1]["image"] = f"{REPO}:latest"
        self.assertFails(check(r, report, self.layout), "not an image by digest")

    def test_an_image_outside_the_push_repo_is_refused(self) -> None:
        r, report = standard(self.layout)
        image = str(r.jobs[1]["image"])
        r.jobs[1]["image"] = image.replace(REPO, "registry.example.com/elsewhere/optimize")
        self.assertFails(check(r, report, self.layout), "not in the run's push repository")

    def test_an_image_the_registry_never_got_is_refused(self) -> None:
        r, report = standard(self.layout)
        r.jobs[1]["image"] = f"{REPO}@sha256:{'a' * 64}"
        self.assertFails(check(r, report, self.layout), "is not in")

    def test_a_job_that_ran_another_images_binary_is_refused(self) -> None:
        r, report = standard(self.layout)
        r.jobs[3]["image"] = r.jobs[2]["image"]
        self.assertFails(check(r, report, self.layout), "is not the /bench in")

    def test_an_image_index_resolves_to_its_amd64_manifest(self) -> None:
        r, report = standard(self.layout)
        image = str(r.jobs[3]["image"])
        r.jobs[3]["image"] = f"{REPO}@{self.layout.index(image.partition('@')[2])}"
        self.assertTrue(check(r, report, self.layout).passed)

    def test_inflated_ops_in_the_report_are_refused(self) -> None:
        r, report = standard(self.layout)
        r.jobs[3]["ops_per_s"] = float(r.jobs[3]["ops_per_s"]) * 3  # type: ignore[arg-type]
        self.assertFails(check(r, report, self.layout), "claims ops_per_s")

    def test_an_inflated_speedup_is_refused(self) -> None:
        r, _ = standard(self.layout)
        self.assertFails(check(r, r.report("ol-r2-prealloc", 300.0), self.layout), "claims 300.00x")

    def test_a_best_job_that_is_not_the_fastest_is_refused(self) -> None:
        r, _ = standard(self.layout)
        self.assertFails(check(r, r.report("ol-r1-map", 50.0), self.layout), "calls ol-r1-map best")

    def test_a_speedup_under_the_threshold_fails(self) -> None:
        r, report = standard(self.layout)
        params = {"gpu": False, "rounds": 3, "min_speedup": 150}
        self.assertFails(check(r, report, self.layout, params), "under the 150x threshold")

    def test_a_run_with_no_parallel_round_fails(self) -> None:
        r = Run(self.layout)
        r.job(0, "ol-base", p50_ms=2000, start_s=0, wall_s=14)
        r.job(1, "ol-r1-a", p50_ms=40, start_s=30, wall_s=4)
        r.job(1, "ol-r1-b", p50_ms=50, start_s=40, wall_s=4)
        self.assertFails(check(r, r.report("ol-r1-a", 50.0), self.layout), "at the same time")

    def test_a_job_of_another_run_is_refused(self) -> None:
        r, report = standard(self.layout)
        r.evidence["list"]["workloads"][2]["run"] = "run-someone-else"  # type: ignore[index]
        self.assertFails(check(r, report, self.layout), "belongs to run 'run-someone-else'")

    def test_a_job_kueue_never_saw_is_refused(self) -> None:
        r, report = standard(self.layout)
        del r.evidence["jobs"]["ol-r1-sort"]  # type: ignore[attr-defined]
        self.assertFails(check(r, report, self.layout), "no record of it")

    def test_a_failed_job_is_refused(self) -> None:
        r, report = standard(self.layout)
        r.evidence["jobs"]["ol-r1-sort"]["status"]["object_status"]["succeeded"] = 0  # type: ignore[index]
        self.assertFails(check(r, report, self.layout), "did not succeed")

    def test_a_log_claiming_more_work_than_its_job_lasted_is_refused(self) -> None:
        r, report = standard(self.layout)
        r.set_result("ol-base", r.result("ol-base") | {"total_ms": 60_000})
        self.assertFails(check(r, report, self.layout), "of work in a 14.0s Job")

    def test_a_fast_claim_from_a_job_as_slow_as_the_baseline_is_refused(self) -> None:
        r = Run(self.layout)
        r.job(0, "ol-base", p50_ms=2000, start_s=0, wall_s=14)
        r.job(1, "ol-r1-a", p50_ms=40, start_s=30, wall_s=14)
        r.job(1, "ol-r1-b", p50_ms=50, start_s=31, wall_s=14)
        verdict = check(r, r.report("ol-r1-a", 50.0), self.layout)
        self.assertFails(verdict, "but its Job ran only 0.0s shorter")

    def test_a_shrunk_workload_is_refused(self) -> None:
        r, report = standard(self.layout)
        result = r.result("ol-r1-map")
        smaller = result | {
            "records": 100,
            "checksum": v.reference_checksum(int(str(result["seed"])), 100),
        }
        r.set_result("ol-r1-map", smaller)
        r.jobs[1]["checksum"] = smaller["checksum"]
        self.assertFails(check(r, report, self.layout), "not the fixed workload")

    def test_a_seed_not_from_the_pod_name_is_refused(self) -> None:
        r, report = standard(self.layout)
        r.set_result("ol-r1-map", r.result("ol-r1-map") | {"seed": "1"})
        self.assertFails(check(r, report, self.layout), "its seed")

    def test_a_result_naming_another_pod_is_refused(self) -> None:
        r, report = standard(self.layout)
        r.set_result("ol-r1-map", r.result("ol-r1-map") | {"pod": "ol-base-x7k2p"})
        self.assertFails(check(r, report, self.layout), "names pod 'ol-base-x7k2p'")

    def test_a_tampered_harness_fails(self) -> None:
        r, report = standard(self.layout)
        self.assertFails(check(r, report, self.layout, tampered=True), "app/main.go differs")

    def test_too_many_rounds_fail(self) -> None:
        r, _ = standard(self.layout)
        r.job(4, "ol-r4", p50_ms=30, start_s=90, wall_s=3)
        params = {"gpu": False, "rounds": 3, "min_speedup": 5}
        self.assertFails(check(r, r.report("ol-r2-prealloc", 100.0), self.layout, params), "1..3")

    def test_two_baselines_fail(self) -> None:
        r, _ = standard(self.layout)
        r.job(0, "ol-base-2", p50_ms=2000, start_s=0, wall_s=14)
        self.assertFails(check(r, r.report("ol-r2-prealloc", 100.0), self.layout), "round 0")

    def test_the_gpu_variant_compares_checksums_with_the_baseline(self) -> None:
        r, report = standard(self.layout)
        gpu = {"gpu": True, "rounds": 3, "min_speedup": 5}
        for name in ("ol-base", "ol-r1-map", "ol-r1-sort", "ol-r2-prealloc"):
            res = r.result(name) | {"records": v.GPU_ROWS, "checksum": "c" * 64}
            r.set_result(name, res)
        for claim in r.jobs:
            claim["checksum"] = "c" * 64
        self.assertTrue(check(r, report, self.layout, gpu).passed)
        r.set_result("ol-r2-prealloc", r.result("ol-r2-prealloc") | {"checksum": "mismatch"})
        r.jobs[3]["checksum"] = "mismatch"
        verdict = check(r, report, self.layout, gpu)
        self.assertEqual(verdict.rejected, {"ol-r2-prealloc": "wrong answer"})

    def test_params_keep_their_declared_types(self) -> None:
        self.assertEqual(v.Params.parse({}), v.Params(variant="cpu", rounds=3, min_speedup=5))
        self.assertEqual(
            v.Params.parse({"gpu": True, "rounds": 2, "min_speedup": 2.5}),
            v.Params(variant="gpu", rounds=2, min_speedup=2.5),
        )
        for bad in ({"gpu": "true"}, {"rounds": "3"}, {"rounds": True}, {"min_speedup": "5"}):
            with self.assertRaises(v.Invalid, msg=str(bad)):
                v.Params.parse(bad)

    def test_a_malformed_report_is_invalid(self) -> None:
        for bad in (
            [],
            {"jobs": []},
            {"jobs": [{"round": 0, "job": "Bad_Name", "image": "x", "checksum": "c",
                       "ops_per_s": 1, "p50_ms": 1}], "best_job": "Bad_Name", "speedup": 1},
            {"jobs": [{"round": "0", "job": "a", "image": "x", "checksum": "c",
                       "ops_per_s": 1, "p50_ms": 1}], "best_job": "a", "speedup": 1},
            {"jobs": [{"round": 0, "job": "a", "image": "x", "checksum": "c",
                       "ops_per_s": 1, "p50_ms": 1}], "best_job": "b", "speedup": 1},
        ):
            with self.assertRaises(v.Invalid, msg=json.dumps(bad)):
                v.Report.parse(bad)


class CommandLineTest(unittest.TestCase):
    """verify.py as the playbook runs it: in a workspace, reading staged inputs and the env."""

    def setUp(self) -> None:
        self.dir = Path(tempfile.mkdtemp())
        self.layout = Layout(self.dir / "oci")
        self.ws = self.dir / "ws"
        r, report = standard(self.layout, records=v.RECORDS)
        files = {
            "app/main.go": (PACK / "app/main.go").read_bytes(),
            "inputs/optimize/app/main.go": (PACK / "app/main.go").read_bytes(),
            "inputs/optimize/report.json": json.dumps(report).encode(),
            "inputs/collect/evidence/kueue.json": json.dumps(r.evidence).encode(),
            "inputs.json": json.dumps(
                {"params": {"gpu": False, "rounds": 3, "min_speedup": 5}}
            ).encode(),
        }
        for path, data in files.items():
            (self.ws / path).parent.mkdir(parents=True, exist_ok=True)
            (self.ws / path).write_bytes(data)
        servers = [
            {
                "name": "buildit",
                "url": "http://host.openshell.internal:8848/mcp",
                "scopes": [],
                "transport": {
                    "local": {"buildit": {"run_issuer": "a=b", "namespace": "n", "push_repo": REPO}}
                },
            }
        ]
        self.env = os.environ | {
            "CRUCIBLE_INPUTS_FILE": str(self.ws / "inputs.json"),
            "CRUCIBLE_AGENT_SERVERS": json.dumps(servers),
            "HOSTNAME": RUN,
        }

    def tearDown(self) -> None:
        shutil.rmtree(self.dir)

    def run_verify(self, *args: str) -> dict[str, object]:
        done = subprocess.run(
            [sys.executable, str(PACK / "verify/verify.py"), *args],
            cwd=self.ws,
            env=self.env,
            capture_output=True,
            text=True,
            check=True,
        )
        return json.loads(done.stdout.strip().splitlines()[-1])

    def test_the_gate_reads_only_the_report(self) -> None:
        self.assertEqual(self.run_verify("gate"), {"status": "pass", "jobs": 4})
        (self.ws / "inputs/optimize/report.json").write_text("{}")
        gated = self.run_verify("gate")
        self.assertEqual(gated["status"], "fail")
        self.assertIs(gated["pass"], False)

    def test_verify_passes_an_honest_workspace(self) -> None:
        line = self.run_verify("verify", "--oci-layout", str(self.layout.root))
        self.assertIs(line["pass"], True, line)
        self.assertAlmostEqual(float(line["score"]), 100.0)  # type: ignore[arg-type]

    def test_verify_fails_an_edited_harness(self) -> None:
        submitted = self.ws / "inputs/optimize/app/main.go"
        submitted.write_bytes(submitted.read_bytes() + b"\n// faster, trust me\n")
        line = self.run_verify("verify", "--oci-layout", str(self.layout.root))
        self.assertIs(line["pass"], False)
        self.assertIn("app/main.go differs from the pack's copy", line["failures"])

    def test_verify_fails_a_run_whose_jobs_belong_to_another_loop_pod(self) -> None:
        self.env["HOSTNAME"] = "run-other"
        line = self.run_verify("verify", "--oci-layout", str(self.layout.root))
        self.assertIs(line["pass"], False)


class PiecesTest(unittest.TestCase):
    def test_the_push_repo_comes_from_the_buildit_grant(self) -> None:
        servers = [
            {"name": "kueue", "url": "http://k/mcp", "scopes": [], "transport": "remote"},
            {
                "name": "buildit",
                "url": "http://host.openshell.internal:8848/mcp",
                "scopes": [],
                "transport": {
                    "local": {"buildit": {"run_issuer": "a=b", "namespace": "n", "push_repo": REPO}}
                },
            },
        ]
        self.assertEqual(v.push_repo_from_env(json.dumps(servers)), REPO)
        with self.assertRaises(v.Invalid):
            v.push_repo_from_env(json.dumps(servers[:1]))
        with self.assertRaises(v.Invalid):
            v.push_repo_from_env("")

    def test_the_authfile_entry_is_the_longest_matching_prefix(self) -> None:
        authfile = {
            "auths": {
                "registry.example.com": {"auth": "host"},
                "https://registry.example.com/team": {"auth": "team"},
                "registry.example.com/tea": {"auth": "not-a-path-prefix"},
                "other.example.com": {"auth": "other"},
            }
        }
        self.assertEqual(v.basic_auth(authfile, REPO), "team")
        self.assertEqual(v.basic_auth(authfile, "registry.example.com/x/y"), "host")
        self.assertIsNone(v.basic_auth(authfile, "third.example.com/a"))
        self.assertIsNone(v.basic_auth({}, REPO))

    def test_a_bearer_challenge_parses(self) -> None:
        scheme, params = v.parse_challenge(
            'Bearer realm="https://registry.example.com/v2/auth",service="registry.example.com"'
        )
        self.assertEqual(scheme, "bearer")
        self.assertEqual(params["realm"], "https://registry.example.com/v2/auth")
        self.assertEqual(params["service"], "registry.example.com")
        self.assertEqual(v.parse_challenge('Basic realm="x"')[0], "basic")

    def test_a_layer_whose_bytes_do_not_match_its_digest_is_refused(self) -> None:
        root = Path(tempfile.mkdtemp())
        try:
            layout = Layout(root)
            digest = layout.image({"bench": b"binary"})
            manifest = json.loads((root / "blobs/sha256" / digest[7:]).read_text())
            layer = root / "blobs/sha256" / manifest["layers"][0]["digest"][7:]
            layer.write_bytes(gzip.compress(b"swapped"))
            with self.assertRaisesRegex(v.Invalid, "does not hash to its digest"):
                v.image_files(v.LayoutRegistry(root), v.Image(REPO, digest), "bench")
        finally:
            shutil.rmtree(root)


@unittest.skipUnless(shutil.which("go"), "needs go")
class HarnessTest(unittest.TestCase):
    """The Python reference and app/ agree, and the verifier's checks hold for the real binary."""

    def test_the_reference_matches_the_go_harness(self) -> None:
        out = Path(tempfile.mkdtemp())
        try:
            binary = out / "bench"
            subprocess.run(
                ["go", "build", "-o", str(binary), "."],
                cwd=PACK / "app",
                check=True,
                env=os.environ | {"CGO_ENABLED": "0"},
            )
            run = subprocess.run(
                [str(binary)],
                check=True,
                capture_output=True,
                text=True,
                env=os.environ | {"BENCH_RECORDS": str(RECORDS)},
            )
            line = [ln for ln in run.stdout.splitlines() if ln.startswith(v.RESULT_PREFIX)]
            self.assertEqual(len(line), 1, run.stdout)
            result = json.loads(line[0][len(v.RESULT_PREFIX) :])
            pod = str(result["pod"])
            self.assertEqual(result["seed"], str(v.pod_seed(pod)))
            self.assertEqual(result["checksum"], v.reference_checksum(v.pod_seed(pod), RECORDS))
            self.assertEqual(result["exe_sha256"], hashlib.sha256(binary.read_bytes()).hexdigest())
            self.assertEqual(result["records"], RECORDS)
            self.assertEqual(result["iterations"], v.ITERATIONS)
        finally:
            shutil.rmtree(out)


if __name__ == "__main__":
    unittest.main()
