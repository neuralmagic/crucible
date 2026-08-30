#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Find workflow uses references that are not full commit-SHA pins."""

import json
import os
import re
import sys
from pathlib import Path
from typing import TypedDict


USES = re.compile(r"^\s*(?:-\s*)?uses\s*:\s*(\S+)")
FULL_SHA = re.compile(r"^[0-9a-fA-F]{40}$")


class Finding(TypedDict):
    workflow: str
    line: int
    uses: str
    action: str
    ref: str
    kind: str
    detail: str


def load_workflows(path: Path) -> tuple[str, str, dict[str, str]]:
    try:
        payload = json.loads(path.read_text())
    except (FileNotFoundError, json.JSONDecodeError) as exc:
        print(f"cannot read captured workflows: {exc}", file=sys.stderr)
        sys.exit(1)
    if not isinstance(payload, dict):
        print("WORKFLOWS.json must be an object", file=sys.stderr)
        sys.exit(1)
    repo, ref = payload.get("repo"), payload.get("ref")
    raw = payload.get("workflows")
    if not isinstance(repo, str) or not isinstance(ref, str) or not isinstance(raw, list):
        print("WORKFLOWS.json has an invalid shape", file=sys.stderr)
        sys.exit(1)
    workflows: dict[str, str] = {}
    for item in raw:
        if isinstance(item, dict) and isinstance(item.get("path"), str) and isinstance(item.get("content"), str):
            workflows[item["path"]] = item["content"]
    return repo, ref, workflows


def value_without_comment(value: str) -> str:
    return value.split(" #", 1)[0].strip().strip("'\"")


def inspect(path: str, text: str) -> tuple[list[Finding], int]:
    findings: list[Finding] = []
    checked = 0
    for number, line in enumerate(text.splitlines(), start=1):
        match = USES.match(line)
        if not match:
            continue
        checked += 1
        value = value_without_comment(match.group(1))
        if "@" not in value:
            continue
        action, action_ref = value.rsplit("@", 1)
        if not action or not action_ref or FULL_SHA.fullmatch(action_ref):
            continue
        kind = "dynamic" if "${{" in action_ref or "}}" in action_ref else "tag-or-branch"
        findings.append(
            Finding(
                workflow=path,
                line=number,
                uses=value,
                action=action,
                ref=action_ref,
                kind=kind,
                detail="not a full 40-character commit SHA",
            )
        )
    return findings, checked


def main() -> None:
    inputs: dict[str, object] = json.loads(os.environ.get("CRUCIBLE_INPUTS", "{}"))
    item = inputs.get("item")
    if not isinstance(item, str) or not item:
        print("no workflow item in CRUCIBLE_INPUTS", file=sys.stderr)
        sys.exit(1)

    repo, ref, workflows = load_workflows(Path("inputs/scan/WORKFLOWS.json"))
    if item not in workflows:
        print(f"workflow {item!r} is absent from WORKFLOWS.json", file=sys.stderr)
        sys.exit(1)
    findings, checked = inspect(item, workflows[item])
    rows = [
        f"| {finding['line']} | `{finding['uses']}` | {finding['kind']} | {finding['detail']} |"
        for finding in findings
    ]
    lines = [
        f"# Action pins in {item}",
        "",
        f"{len(findings)} unpinned `uses:` reference(s), checked against `{repo}@{ref}`.",
        "",
        "| line | uses | classification | detail |",
        "| --- | --- | --- | --- |",
        *(rows or ["| — | — | none | every `uses:` reference is a full commit SHA or local |"]),
        "",
    ]
    Path("CHECK.md").write_text("\n".join(lines))
    Path("FINDINGS.json").write_text(
        json.dumps({"repo": repo, "ref": ref, "workflow": item, "unpinned": findings}, indent=2) + "\n"
    )
    print(json.dumps({"unpinned": str(len(findings)), "checked": str(checked)}))


if __name__ == "__main__":
    main()
