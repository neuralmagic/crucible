#!/usr/bin/env bash
# End-to-end proof of a revise loop through a real OpenShell sandbox on local podman, with no
# model: examples/revise-loop's `claude` is tools/fake-agent.py behind Claude Code's argv and
# stream-json result. The reviewer rejects the first draft and accepts the revision, so the run
# must report two rounds, resume the author's session, and commit both drafts.
#
# Needs podman (a running `podman machine` on macOS), openshell, and openshell-gateway on PATH.
# CRUCIBLE_BIN overrides the engine binary; KEEP=1 keeps the run directory.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=localhost/crucible-fake-claude:dev
WORK_DIR=$(mktemp -d)
if [ "${KEEP:-0}" = 1 ]; then
    echo "run directory: $WORK_DIR"
else
    trap 'rm -rf "$WORK_DIR"' EXIT
fi

CRUCIBLE=${CRUCIBLE_BIN:-}
if [ -z "$CRUCIBLE" ]; then
    cargo build -q -p crucible --manifest-path "$ROOT/Cargo.toml"
    CRUCIBLE="$ROOT/target/debug/crucible"
fi

if [ -z "${OPENSHELL_PODMAN_SOCKET:-}" ] && podman machine inspect >/dev/null 2>&1; then
    OPENSHELL_PODMAN_SOCKET=$(podman machine inspect --format '{{.ConnectionInfo.PodmanSocket.Path}}')
    export OPENSHELL_PODMAN_SOCKET
fi
# The claude harness resolves credentials before the sandbox exists; a key skips Vertex.
export ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY:-fake}

echo "==> building $IMAGE"
cp -R "$ROOT/examples/revise-loop/image" "$WORK_DIR/image"
cp "$ROOT/tools/fake-agent.py" "$WORK_DIR/image/"
podman build -q -t "$IMAGE" "$WORK_DIR/image" >/dev/null

echo "==> running examples/revise-loop"
cp -R "$ROOT/examples/revise-loop" "$WORK_DIR/pack"
cd "$WORK_DIR/pack"
"$CRUCIBLE" plan run --manifest crucible.toml --compute-driver podman \
    --max-cost 1 --max-time 15m >run.out 2>run.err || {
    tail -40 run.err
    exit 1
}

echo "==> checking the run"
python3 - <<'PY'
import json
import pathlib
import subprocess

rows = []
for line in pathlib.Path("state/session.jsonl").read_text().splitlines():
    try:
        event = json.loads(line)
    except ValueError:
        continue
    if event.get("kind") == "task_result":
        rows.append((event["task"], event["status"]))

expected = [
    ("author[round-1]", "pass"),
    ("review[round-1]", "fail"),
    ("author[round-2]", "pass"),
    ("review[round-2]", "pass"),
    ("author", "pass"),
    ("review", "pass"),
]
assert rows == expected, f"task rows {rows} != {expected}"

session = pathlib.Path("workspace/SESSION.log").read_text()
assert session == "start\nresume\n", f"the revision did not resume the session: {session!r}"

probe = pathlib.Path("state/files/author/PROBE.md").read_text()
assert '"revision"' in probe, "the revision was not handed the review"

subjects = subprocess.run(
    ["git", "-C", "workspace", "log", "--format=%s"], capture_output=True, text=True, check=True
).stdout.splitlines()
assert subjects.count("task author") == 2, f"expected two author commits: {subjects}"

print("ok: two rounds, resumed session, revision committed")
PY
