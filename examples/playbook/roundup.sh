#!/bin/sh
# The lossy join. `join = "passed"` dispatches this once every auditor is terminal and
# hands over only the ones that passed, so the advisory auditor's failure costs the run
# its finding, not its verdict.
python3 - <<'PY'
import json, os

inputs = json.loads(os.environ.get("CRUCIBLE_INPUTS", "{}"))
expected = ["audit-headings", "audit-bullets", "audit-freshness"]
findings = [f for r in inputs.values() for f in r.get("findings", [])]
print(json.dumps({
    "reporting": sorted(inputs),
    "silent": [name for name in expected if name not in inputs],
    "findings": findings,
}))
PY
