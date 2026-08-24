"""Assemble the report and the filing intent from what the probes and triage instances captured."""

import json
import os
from pathlib import Path


def outputs_of(entry: object) -> dict[str, dict[str, str]]:
    if isinstance(entry, dict):
        inner = entry.get("result", entry)
        if isinstance(inner, dict):
            outputs = inner.get("outputs", {})
            if isinstance(outputs, dict):
                return {
                    key: {k: v for k, v in res.items() if isinstance(v, str)}
                    for key, res in outputs.items()
                    if isinstance(res, dict)
                }
    return {}


def issue_payloads(dirty: list[str], triaged: dict[str, dict[str, str]]) -> list[dict]:
    groups: dict[tuple[str, str], dict] = {}
    for variant in dirty:
        drafted = {}
        payload = Path(f"inputs/triage[{variant}]/ISSUE.json")
        if payload.exists():
            try:
                drafted = json.loads(payload.read_text())
            except json.JSONDecodeError:
                drafted = {}
        repo = drafted.get("repo")
        blocker = triaged.get(variant, {}).get("blocker") or "unknown"
        if not repo:
            continue
        group = groups.setdefault(
            (repo, blocker),
            {
                "repo": repo,
                "title": drafted.get("title") or f"FIPS: {blocker} reaches the compiled graph",
                "body": drafted.get("body", ""),
                "dedupe_key": f"{blocker}",
                "variants": [],
            },
        )
        group["variants"].append(variant)

    payloads = []
    for group in groups.values():
        variants = sorted(group.pop("variants"))
        group["body"] = f"{group['body']}\n\nAffected variants: {', '.join(variants)}\n"
        payloads.append(group)
    return payloads


def main() -> None:
    inputs = json.loads(os.environ.get("CRUCIBLE_INPUTS", "{}"))
    probes = outputs_of(inputs.get("probe", {}))
    triaged = outputs_of(inputs.get("triage", {}))

    clean = sorted(k for k, v in probes.items() if v.get("status") == "clean")
    dirty = sorted(k for k, v in probes.items() if v.get("status") == "dirty")

    lines = ["# FIPS watch", ""]
    rev = Path("WATCHED_REV")
    if rev.exists():
        lines += [f"Watched revision: `{rev.read_text().strip()}`", ""]
    lines += [f"- clean variants: {len(clean)}", f"- dirty variants: {len(dirty)}", ""]

    scanned = Path("inputs/scan/VARIANTS.md")
    if scanned.exists():
        lines += [scanned.read_text().strip(), ""]

    lines += ["| variant | status | blockers |", "| --- | --- | --- |"]
    for key in sorted(probes):
        v = probes[key]
        lines.append(f"| {key} | {v.get('status','?')} | {v.get('blockers') or '—'} |")
    lines.append("")

    for key in dirty:
        for name in (f"inputs/probe[{key}]/PROBE.md", f"inputs/triage[{key}]/TRIAGE.md"):
            detail = Path(name)
            if detail.exists():
                lines += [detail.read_text().strip(), ""]

    Path("REPORT.md").write_text("\n".join(lines) + "\n")

    payloads = issue_payloads(dirty, triaged)
    Path("ISSUES.json").write_text(json.dumps(payloads, indent=2) + "\n")

    print(json.dumps({"clean": len(clean), "dirty": len(dirty)}))


if __name__ == "__main__":
    main()
