"""Fold the pipeline's result fields and its staged artifact into REPORT.md."""

import json
import os
import sys
from pathlib import Path


def inputs() -> dict:
    path = os.environ.get("CRUCIBLE_INPUTS_FILE")
    raw = Path(path).read_text() if path else os.environ.get("CRUCIBLE_INPUTS", "{}")
    return json.loads(raw)


def main() -> None:
    result = inputs()["pipeline"]
    notes = Path("inputs", "pipeline", "out", "notes.md")
    if not notes.is_file():
        print(json.dumps({"status": "fail", "note": f"{notes} was not staged"}))
        sys.exit(0)
    text = notes.read_text()
    Path("REPORT.md").write_text(
        f"# {result['message']}\n\n"
        f"- count: {result['count']}\n"
        f"- targets: {', '.join(result['targets'])}\n\n"
        f"## Pipeline notes\n\n{text}"
    )
    print(
        json.dumps(
            {
                "message": result["message"],
                "count": result["count"],
                "notes_lines": len(text.splitlines()),
            }
        )
    )


main()
