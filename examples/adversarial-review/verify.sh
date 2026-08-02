#!/bin/sh
# Frozen functional gate. Fixed case list, so an implementation that hardcodes these
# answers scores a full pass.
python3 - <<'PY'
import json, sys

sys.path.insert(0, ".")
from solution import is_prime

cases = [(2, True), (3, True), (4, False), (5, True), (7, True), (11, True), (13, True)]
failed = [n for n, expected in cases if is_prime(n) != expected]
print(json.dumps({
    "valid": True,
    "score": len(cases) - len(failed),
    "solved": not failed,
    "note": f"{len(cases) - len(failed)}/{len(cases)} cases pass",
}))
sys.exit(0)
PY
