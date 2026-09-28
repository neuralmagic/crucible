"""Assemble REPORT.md from what the passing triage instances captured, and ask for one fix run
per confirmed bug."""

import json
import os
import re
from pathlib import Path


def outputs_of(entry: object) -> dict[str, dict[str, str]]:
    if isinstance(entry, dict):
        inner = entry.get("result", entry)
        if isinstance(inner, dict):
            outputs = inner.get("outputs", {})
            if isinstance(outputs, dict):
                return {
                    issue: {k: v for k, v in result.items() if isinstance(v, str)}
                    for issue, result in outputs.items()
                    if isinstance(result, dict)
                }
    return {}


FIX_WORKFLOW = "issue-fix"
FIX_SEVERITIES = {"critical", "high"}
REPO = re.compile(r"[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9._-]+")


def repo_of(entry: object) -> str:
    if isinstance(entry, dict):
        repo = entry.get("repo")
        if isinstance(repo, str) and REPO.fullmatch(repo):
            return repo
    return ""


def fix_asks(repo: str, rows: list[tuple[str, dict[str, str]]]) -> list[dict[str, object]]:
    """One ask per bug a triage instance was confident enough to call severe. The key names the
    issue itself, so the same bug found by next week's sweep is recognized as a repeat."""
    if not repo:
        return []
    return [
        {
            "key": f"{repo}#{issue}",
            "workflow": FIX_WORKFLOW,
            "params": {"repo": repo, "issue": issue},
        }
        for issue, result in rows
        if issue.isdigit()
        and result.get("classification") == "bug"
        and result.get("severity") in FIX_SEVERITIES
        and result.get("confidence") == "high"
    ]


def main() -> None:
    inputs = json.loads(os.environ.get("CRUCIBLE_INPUTS", "{}"))
    rows = sorted(
        outputs_of(inputs.get("triage", {})).items(),
        key=lambda row: int(row[0]) if row[0].isdigit() else 0,
        reverse=True,
    )

    lines = ["# Triage report", ""]
    scanned = Path("inputs/scan/ISSUES.md")
    if scanned.exists():
        lines += [scanned.read_text().strip(), ""]

    lines += [
        "| issue | classification | severity | confidence |",
        "| --- | --- | --- | --- |",
    ]
    for issue, result in rows:
        lines.append(
            "| {} | {} | {} | {} |".format(
                issue,
                result.get("classification", "?"),
                result.get("severity", "?"),
                result.get("confidence", "?"),
            )
        )
    lines.append("")

    asks = fix_asks(repo_of(inputs.get("scan", {})), rows)
    if asks:
        lines += [f"Asked for a fix run (`{FIX_WORKFLOW}`) on:", ""]
        lines += [f"- {ask['key']}" for ask in asks]
        lines.append("")

    for issue, _ in rows:
        detail = Path("inputs") / f"triage[{issue}]" / "TRIAGE.md"
        if detail.exists():
            lines += ["---", "", detail.read_text().strip(), ""]

    Path("REPORT.md").write_text("\n".join(lines))
    print(json.dumps({"triaged": len(rows), "asks": asks}))


if __name__ == "__main__":
    main()
