#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Assemble the action-pins report and grounded proposal input."""

import json
import os
from pathlib import Path
from typing import Any


def outputs_of(entry: object) -> dict[str, dict[str, str]]:
    if isinstance(entry, dict):
        inner = entry.get("result", entry)
        if isinstance(inner, dict) and isinstance(inner.get("outputs"), dict):
            return {
                key: {name: value for name, value in result.items() if isinstance(value, str)}
                for key, result in inner["outputs"].items()
                if isinstance(result, dict)
            }
    return {}


def number(value: str) -> int:
    try:
        return int(value)
    except ValueError:
        return 0


def captured(workflow: str) -> dict[str, Any]:
    path = Path(f"inputs/check[{workflow}]/FINDINGS.json")
    try:
        payload = json.loads(path.read_text())
    except (FileNotFoundError, json.JSONDecodeError):
        return {}
    return payload if isinstance(payload, dict) else {}


def scanned() -> dict[str, Any]:
    try:
        payload = json.loads(Path("inputs/scan/WORKFLOWS.json").read_text())
    except (FileNotFoundError, json.JSONDecodeError):
        return {}
    return payload if isinstance(payload, dict) else {}


def main() -> None:
    inputs: dict[str, object] = json.loads(os.environ.get("CRUCIBLE_INPUTS", "{}"))
    results = sorted(outputs_of(inputs.get("check", {})).items())
    workflows = [workflow for workflow, _ in results]
    scan = scanned()
    if not workflows and isinstance(scan.get("workflows"), list):
        workflows = [
            item["path"]
            for item in scan["workflows"]
            if isinstance(item, dict) and isinstance(item.get("path"), str)
        ]
    payloads = [captured(workflow) for workflow in workflows]
    first = next((payload for payload in payloads if payload.get("repo")), scan)
    findings = [
        finding
        for payload in payloads
        for finding in payload.get("unpinned", [])
        if isinstance(finding, dict)
    ]
    checked = sum(number(result.get("checked", "0")) for _, result in results)
    repo = str(first.get("repo", ""))
    ref = str(first.get("ref", "main"))
    output = {"repo": repo, "ref": ref, "workflows": workflows, "unpinned": findings}
    Path("FINDINGS.json").write_text(json.dumps(output, indent=2) + "\n")

    lines = [
        "# GitHub Actions pinning report",
        "",
        f"Repository: `{repo}@{ref}`",
        "",
        f"{len(findings)} unpinned reference(s) found across {len(workflows)} workflow(s); {checked} `uses:` line(s) checked.",
        "",
        "| workflow | line | uses | classification |",
        "| --- | --- | --- | --- |",
    ]
    for finding in findings:
        lines.append(
            f"| {finding.get('workflow', '')} | {finding.get('line', '')} | "
            f"`{finding.get('uses', '')}` | {finding.get('kind', '')} |"
        )
    if not findings:
        lines.append("| — | — | — | all references are full commit SHAs or local |")
    lines.extend(["", "## Per-workflow checks", ""])
    for workflow in workflows:
        detail = Path(f"inputs/check[{workflow}]/CHECK.md")
        if detail.exists():
            lines.extend([detail.read_text().strip(), ""])
    Path("REPORT.md").write_text("\n".join(lines) + "\n")
    print(json.dumps({"unpinned": str(len(findings)), "checked": str(checked), "workflows": str(len(workflows))}))


if __name__ == "__main__":
    main()
