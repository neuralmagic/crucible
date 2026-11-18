"""Reads the climb's report, its timeline and each Job's result line, and writes summary.md: every
candidate's time and correctness, the speedup of the kept one, how long each iteration took from
staging to its measurement, and how much of each build was reused. The last stdout line emits the
same numbers. Fails when the baseline was not measured or the kept candidate is not correct."""

import json
import os
import statistics
import sys
from pathlib import Path
from typing import Any

PREFIX = "hill-climb-result "
SERVE = "hill-climb-serve "


def results(path: Path) -> dict[str, str]:
    """Each label's recorded Job log, the newest record winning."""
    out: dict[str, str] = {}
    if path.is_file():
        for text in path.read_text().splitlines():
            if text.strip():
                row = json.loads(text)
                out[row["label"]] = row["log"]
    return out


def line(log: str | None, prefix: str) -> dict[str, Any] | None:
    for text in (log or "").splitlines():
        at = text.find(prefix)
        if at >= 0:
            return json.loads(text[at + len(prefix):])
    return None


def stamps(timeline: Path) -> dict[str, dict[str, float]]:
    out: dict[str, dict[str, float]] = {}
    if timeline.is_file():
        for text in timeline.read_text().splitlines():
            if text.strip():
                row = json.loads(text)
                out.setdefault(row["label"], {})[row["event"]] = row["t"]
    return out


def span(times: dict[str, float], start: str, end: str) -> float | None:
    if start in times and end in times:
        return round(times[end] - times[start], 1)
    return None


def median(values: list[float | None]) -> float | None:
    present = [v for v in values if v is not None]
    return round(statistics.median(present), 1) if present else None


def summarize(root: Path, seed: dict[str, Any]) -> dict[str, Any]:
    report = json.loads((root / "report.json").read_text())
    times = stamps(root / "timeline.jsonl")
    logs = results(root / "results.jsonl")
    rows = []
    for cand in report.get("candidates", []):
        label = cand["label"]
        result = line(logs.get(label), PREFIX)
        t = times.get(label, {})
        rows.append(
            {
                "label": label,
                "round": cand.get("round"),
                "recipe": cand.get("recipe"),
                "change": cand.get("change", ""),
                "ok": bool(result and result.get("ok")),
                "geomean_us": result.get("geomean_us") if result else None,
                "build_push_s": span(t, "staged", "pushed"),
                "queue_run_s": span(t, "submitted", "measured"),
                "built": "pushed" in t,
                "iteration_s": None if cand.get("recipe") == "seed" else span(t, "staged", "measured"),
            }
        )
    by_label = {r["label"]: r for r in rows}
    base = by_label.get("baseline")
    if not base or not base["ok"]:
        sys.exit("the baseline has no correct measurement in results.jsonl")
    best = by_label.get(report.get("best", "baseline"))
    if not best or not best["ok"]:
        sys.exit(f"the kept candidate {report.get('best')!r} has no correct measurement")
    for r in rows:
        if r["ok"]:
            r["speedup"] = round(base["geomean_us"] / r["geomean_us"], 4)
    tried = [r for r in rows if r["recipe"] in ("layer", "dockerfile")]
    built = [r for r in tried if r["built"]]
    layered = [r for r in built if r["recipe"] == "layer"]
    return {
        "best": best["label"],
        "speedup": round(base["geomean_us"] / best["geomean_us"], 4),
        "baseline_us": base["geomean_us"],
        "best_us": best["geomean_us"],
        "candidates": rows,
        "builds": len(built),
        "failed_builds": len(tried) - len(built),
        "layer_builds": len(layered),
        "median_iteration_s": {
            "layer": median([r["iteration_s"] for r in layered]),
            "dockerfile": median(
                [r["iteration_s"] for r in built if r["recipe"] == "dockerfile"]
            ),
        },
        "median_build_push_s": {
            "layer": median([r["build_push_s"] for r in layered]),
            "dockerfile": median(
                [r["build_push_s"] for r in built if r["recipe"] == "dockerfile"]
            ),
        },
        "median_queue_run_s": median([r["queue_run_s"] for r in rows]),
        "seed": seed,
        "serve": {
            side: line(logs.get(f"serve-{side}"), SERVE) for side in ("seed", "best")
        },
        "summary": report.get("summary", ""),
    }


def markdown(s: dict[str, Any]) -> str:
    out = [
        f"# vLLM fused MoE hill climb: {s['speedup']}x",
        "",
        f"Kept `{s['best']}`: {s['best_us']} us geomean against the seed's {s['baseline_us']} us.",
        "",
        "| round | label | recipe | ok | geomean us | speedup | build+push s | queue+run s | iteration s |",
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- |",
    ]
    for r in s["candidates"]:
        out.append(
            f"| {r['round']} | {r['label']} | {r['recipe']} | {'yes' if r['ok'] else 'no'} | "
            f"{r['geomean_us']} | {r.get('speedup', '')} | {r['build_push_s']} | "
            f"{r['queue_run_s']} | {r['iteration_s']} |"
        )
    seed = s["seed"]
    out += [
        "",
        f"Builds: {s['builds']} succeeded, {s['layer_builds']} of them layers that compiled"
        f" nothing; {s['failed_builds']} failed.",
        f"Median iteration: {s['median_iteration_s']}; build and push: {s['median_build_push_s']};"
        f" queue and run: {s['median_queue_run_s']} s.",
        f"Seed: {seed.get('seed_build_seconds')} s to build, {seed.get('seed_push_seconds')} s to"
        f" push, ccache {seed.get('seed_ccache')}.",
    ]
    serve = s["serve"]
    if serve.get("seed") and serve.get("best"):
        out += ["", "| serving | seed | best |", "| --- | --- | --- |"]
        for key in ("output_throughput", "median_ttft_ms", "median_tpot_ms"):
            out.append(f"| {key} | {serve['seed'].get(key)} | {serve['best'].get(key)} |")
    out += ["", str(s["summary"]), ""]
    return "\n".join(out)


def main() -> None:
    inputs = json.loads(Path(os.environ["CRUCIBLE_INPUTS_FILE"]).read_text())
    seed = inputs.get("seed", {}) if isinstance(inputs.get("seed"), dict) else {}
    climb = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(".")
    s = summarize(climb, seed)
    Path("summary.md").write_text(markdown(s))
    print(json.dumps({k: s[k] for k in ("speedup", "best", "builds", "layer_builds", "failed_builds",
                                        "median_iteration_s", "median_queue_run_s")}))


if __name__ == "__main__":
    main()
