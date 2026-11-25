"""Check the launch parameters and emit what the digest task needs."""

import hashlib
import json
import os
import sys
from pathlib import Path


def inputs() -> dict:
    path = os.environ.get("CRUCIBLE_INPUTS_FILE")
    raw = Path(path).read_text() if path else os.environ.get("CRUCIBLE_INPUTS", "{}")
    return json.loads(raw)


def fail(note: str) -> None:
    print(json.dumps({"status": "fail", "note": note}))
    sys.exit(0)


def main() -> None:
    params = inputs().get("params", {})
    label = params.get("label")
    config = params.get("config")
    notes = params.get("notes")
    if not isinstance(label, str) or not label:
        fail("label is not a non-empty string")
    if not isinstance(config, dict):
        fail("config is not a JSON object")
    tags = config.get("tags", [])
    threshold = config.get("threshold", 0)
    if not isinstance(tags, list) or not all(isinstance(t, str) for t in tags):
        fail("config.tags is not a list of strings")
    if isinstance(threshold, bool) or not isinstance(threshold, (int, float)):
        fail("config.threshold is not a number")
    if not isinstance(notes, dict) or not isinstance(notes.get("path"), str):
        fail("notes is not a staged file")
    body = Path(notes["path"]).read_bytes()
    digest = "sha256:" + hashlib.sha256(body).hexdigest()
    if digest != notes.get("digest") or len(body) != notes.get("size"):
        fail(f"notes at {notes['path']} does not match its recorded digest and size")
    print(
        json.dumps(
            {
                "label": label,
                "tags": sorted(tags),
                "threshold": threshold,
                "lines": len(body.decode("utf-8").splitlines()),
            }
        )
    )


main()
