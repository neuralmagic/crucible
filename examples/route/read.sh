#!/bin/sh
python3 - "${TICKET:-outage}" <<'PY'
import json, pathlib, sys
print(json.dumps({"ticket": pathlib.Path("tickets", sys.argv[1] + ".txt").read_text().strip()}))
PY
