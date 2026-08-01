#!/bin/sh
# Blocks the run on a negative review. Nonzero exit = measured failure, which
# short-circuits everything downstream. Policy lives here, not in the reviewer.
python3 - <<'PY'
import json, os, sys

inputs = json.loads(os.environ.get("CRUCIBLE_INPUTS", "{}"))
verdict = inputs.get("review", {})
approved = verdict.get("approved") is True
print(json.dumps({"approved": approved, "finding": verdict.get("finding", "")}))
sys.exit(0 if approved else 1)
PY
