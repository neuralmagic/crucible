"""Assemble REPORT.md from what the passing triage instances captured."""

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
                    issue: {k: v for k, v in result.items() if isinstance(v, str)}
                    for issue, result in outputs.items()
                    if isinstance(result, dict)
                }
    return {}


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

    for issue, _ in rows:
        detail = Path("inputs") / f"triage[{issue}]" / "TRIAGE.md"
        if detail.exists():
            lines += ["---", "", detail.read_text().strip(), ""]

    Path("REPORT.md").write_text("\n".join(lines))
    print(json.dumps({"triaged": len(rows)}))


if __name__ == "__main__":
    main()
