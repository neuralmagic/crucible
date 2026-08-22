#!/bin/sh
# The gate between the two scribe turns: the draft's own count must match what it wrote.
# Nonzero exit is a measured failure, and `polish` is downstream, so a mismatched draft
# never reaches the second turn.
python3 - <<'PY'
import json, os, pathlib, sys

inputs = json.loads(os.environ.get("CRUCIBLE_INPUTS", "{}"))
declared = (inputs.get("draft") or {}).get("entries")
notes = pathlib.Path("NOTES.md")
bullets = (
    sum(1 for line in notes.read_text().splitlines() if line.startswith("- "))
    if notes.exists()
    else 0
)
matches = declared == bullets
print(json.dumps({"declared": declared, "bullets": bullets, "matches": matches}))
sys.exit(0 if matches else 1)
PY
