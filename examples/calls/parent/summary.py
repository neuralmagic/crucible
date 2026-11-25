"""Fold the child playbook's and the CI workflow's results and files into SUMMARY.md."""

import json
import os
import sys
from pathlib import Path


def inputs() -> dict:
    path = os.environ.get("CRUCIBLE_INPUTS_FILE")
    raw = Path(path).read_text() if path else os.environ.get("CRUCIBLE_INPUTS", "{}")
    return json.loads(raw)


def staged(producer: str, path: str) -> str:
    file = Path("inputs", producer, path)
    if not file.is_file():
        print(json.dumps({"status": "fail", "note": f"{file} was not staged"}))
        sys.exit(0)
    return file.read_text()


def main() -> None:
    received = inputs()
    child = received["child"]
    ci = received["ci"]
    digest = staged("child", "DIGEST.md")
    notes = staged("ci", "notes.md")
    lines = [f"# Summary: {child['label']}", ""]
    lines += [
        f"- child: {child['words']} words, {child['bullets']} bullets, "
        f"over threshold: {str(child['over_threshold']).lower()}",
        f"- ci: {ci['summary']} ({ci['legs']} legs, keys {', '.join(ci['payload_keys'])})",
        "",
        "## Child digest",
        "",
        digest.rstrip(),
        "",
        "## CI notes",
        "",
        notes.rstrip(),
    ]
    Path("SUMMARY.md").write_text("\n".join(lines) + "\n")
    print(
        json.dumps(
            {
                "label": child["label"],
                "child_words": child["words"],
                "ci_summary": ci["summary"],
                "ci_legs": ci["legs"],
                "ci_payload_keys": ci["payload_keys"],
            }
        )
    )


main()
