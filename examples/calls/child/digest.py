"""Write DIGEST.md from the staged notes and emit the child's result."""

import json
import os
from pathlib import Path


def inputs() -> dict:
    path = os.environ.get("CRUCIBLE_INPUTS_FILE")
    raw = Path(path).read_text() if path else os.environ.get("CRUCIBLE_INPUTS", "{}")
    return json.loads(raw)


def main() -> None:
    received = inputs()
    read = received["read"]
    notes = Path(received["params"]["notes"]["path"]).read_text()
    words = len(notes.split())
    bullets = [line for line in notes.splitlines() if line.lstrip().startswith("- ")]
    lines = [f"# Digest: {read['label']}", ""]
    lines += [f"- tags: {', '.join(read['tags']) or '(none)'}"]
    lines += [f"- words: {words} (threshold {read['threshold']})", ""]
    lines += ["## Bullets", ""] + (bullets or ["(none)"])
    Path("DIGEST.md").write_text("\n".join(lines) + "\n")
    print(
        json.dumps(
            {
                "label": read["label"],
                "words": words,
                "bullets": len(bullets),
                "tags": read["tags"],
                "over_threshold": words > read["threshold"],
            }
        )
    )


main()
