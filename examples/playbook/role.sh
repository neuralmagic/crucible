#!/bin/sh
# Stand-in for every agent task: branches on CRUCIBLE_PROMPT to act as the drafter, the
# polisher, or one of the three auditors. No model, no cost, no network.
set -e

case "$CRUCIBLE_PROMPT" in
*"AUDIT: HEADINGS"*)
    python3 - <<'PY'
import json, pathlib

body = pathlib.Path("NOTES.md").read_text()
findings = [] if body.startswith("# ") else ["NOTES.md has no title heading"]
result = {"topic": "headings", "findings": findings}
pathlib.Path("PLAN_TASK_RESULT.json").write_text(json.dumps(result))
print(json.dumps(result))
PY
    ;;
*"AUDIT: BULLETS"*)
    python3 - <<'PY'
import json, pathlib

findings = [
    f"line {n} is neither blank, a heading, nor a bullet: {line!r}"
    for n, line in enumerate(pathlib.Path("NOTES.md").read_text().splitlines(), 1)
    if line.strip() and not line.startswith(("# ", "- "))
]
result = {"topic": "bullets", "findings": findings}
pathlib.Path("PLAN_TASK_RESULT.json").write_text(json.dumps(result))
print(json.dumps(result))
PY
    ;;
*"AUDIT: FRESHNESS"*)
    # The advisory peer. It wants the previous release's cut line, which this pack never
    # ships, so it writes no result and the task is a measured failure. Advisory: the
    # roundup folds the two auditors that reported and the run stays valid.
    echo "inbox/.last-release is missing: cannot date the entries" >&2
    exit 1
    ;;
*"POLISH NOTES"*)
    python3 - <<'PY'
import json, os, pathlib

mine = os.environ.get("CRUCIBLE_AGENT_SESSION_ID", "")
trail = pathlib.Path("TURNS.txt")
drafted = trail.read_text().split() if trail.exists() else []
notes = pathlib.Path("NOTES.md")
body = notes.read_text()
if not body.startswith("# "):
    notes.write_text("# Release notes\n\n" + body)
with trail.open("a") as f:
    f.write(f"polish {os.environ.get('CRUCIBLE_AGENT_SESSION_ACTION', '-')}\n")
result = {
    "session": os.environ.get("CRUCIBLE_AGENT_SESSION", ""),
    "continued": len(drafted) > 1 and drafted[1] == mine,
    "titled": True,
}
pathlib.Path("PLAN_TASK_RESULT.json").write_text(json.dumps(result))
print(json.dumps(result))
PY
    ;;
*)
    python3 - <<'PY'
import json, os, pathlib

entries = sorted(pathlib.Path("inbox").glob("*.md"))
bullets = "".join(f"- {p.read_text().strip()}\n" for p in entries)
pathlib.Path("NOTES.md").write_text(bullets)
pathlib.Path("TURNS.txt").write_text(
    f"draft {os.environ.get('CRUCIBLE_AGENT_SESSION_ID', '')}\n"
)
result = {"entries": len(entries)}
pathlib.Path("PLAN_TASK_RESULT.json").write_text(json.dumps(result))
print(json.dumps(result))
PY
    ;;
esac
