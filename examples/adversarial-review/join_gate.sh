#!/bin/sh
# Join node: folds the panel's verdicts and applies the weighting.
#
#   correctness  blocking, fail-closed (not approved, or absent => exit 1)
#   copy-edit    advisory (findings recorded, never block)
python3 - <<'PY'
import json, os, sys

inputs = json.loads(os.environ.get("CRUCIBLE_INPUTS", "{}"))
correctness = inputs.get("review-correctness") or {}
copy_edit = inputs.get("review-copy") or {}

blocked = correctness.get("approved") is not True
advisory = copy_edit.get("findings") or []

print(json.dumps({
    "blocked": blocked,
    "correctness": {
        "approved": correctness.get("approved"),
        "finding": correctness.get("finding", ""),
        "counterexample": correctness.get("counterexample"),
    },
    "copy_edit_advisory": advisory,
    "copy_edit_count": len(advisory),
    "reviewers_reporting": sorted(inputs.keys()),
}))
sys.exit(1 if blocked else 0)
PY
