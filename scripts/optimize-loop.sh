#!/usr/bin/env bash
# Launch examples/optimize-loop on a running controller in the cluster whose runs are granted
# buildit (with a push repository) and crucible-kueue, and wait for its verdict. The pack's own
# verify task decides pass or fail; this script prints it and the files the run captured.
#
#   CONTEXT=<kube context> CTL_NS=<controller namespace> [GPU=false] [ROUNDS=3] \
#   [MIN_SPEEDUP=5] [MAX_COST=10] [MAX_TIME=120m] scripts/optimize-loop.sh
#
# The launch is made with the caller's own cluster token, as the kueue e2e makes its own, and the
# controller must already know the caller's Kubernetes user. The script creates nothing in the
# cluster; the Jobs it causes expire by their TTL.
set -euo pipefail

: "${CONTEXT:?set CONTEXT to the kube context of the cluster the controller runs in}"
: "${CTL_NS:?set CTL_NS to the namespace of the crucible-controller Service}"
GPU=${GPU:-false}
ROUNDS=${ROUNDS:-3}
MIN_SPEEDUP=${MIN_SPEEDUP:-5}
MAX_COST=${MAX_COST:-10}
MAX_TIME=${MAX_TIME:-120m}
DRAFT=optimize-loop
TIMEOUT_SECS=${TIMEOUT_SECS:-9000}

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d)
log() { printf '[optimize-loop] %s\n' "$*" >&2; }
fail() { log "FAIL: $*"; exit 1; }

FORWARD_PID=
cleanup() {
    [ -n "$FORWARD_PID" ] && kill "$FORWARD_PID" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

SQLX_OFFLINE=true cargo build -q --manifest-path "$ROOT/Cargo.toml" -p crux --bins
BIN=$(cargo metadata -q --manifest-path "$ROOT/Cargo.toml" --format-version 1 --no-deps \
    | jq -r .target_directory)/debug
TOKEN=$(kubectl config view --raw --minify --context "$CONTEXT" -o jsonpath='{.users[0].user.token}')
[ -n "$TOKEN" ] || fail "context $CONTEXT carries no bearer token to launch with"

kubectl --context "$CONTEXT" -n "$CTL_NS" port-forward svc/crucible-controller :8080 \
    > "$WORK/forward.log" 2>&1 &
FORWARD_PID=$!
PORT=
for _ in $(seq 30); do
    PORT=$(sed -n 's/^Forwarding from 127\.0\.0\.1:\([0-9]*\) .*/\1/p' "$WORK/forward.log" | head -1)
    [ -n "$PORT" ] && break
    sleep 1
done
[ -n "$PORT" ] || fail "the port-forward to the controller never listened: $(cat "$WORK/forward.log")"
printf 'url = "http://127.0.0.1:%s"\n' "$PORT" > "$WORK/crux.toml"
crux() { env -u CONTROLLER_URL CONTROLLER_API_TOKEN="$TOKEN" "$BIN/crux" "$@" --config "$WORK/crux.toml"; }
for _ in $(seq 30); do
    crux whoami >/dev/null 2>&1 && break
    sleep 2
done
log "launching as $(crux whoami | head -1)"

if ! crux draft-files "$DRAFT" --json > "$WORK/draft.json" 2>/dev/null; then
    crux draft-create "$DRAFT" --description "build, run on Kueue, measure, iterate" >/dev/null
    crux draft-files "$DRAFT" --json > "$WORK/draft.json"
fi
BASE=$(jq -r '.version' "$WORK/draft.json")
crux draft-push "$DRAFT" "$ROOT/examples/optimize-loop" --base-version "$BASE" >/dev/null
PARAMS=$(jq -cn --arg gpu "$GPU" --arg rounds "$ROUNDS" --arg min "$MIN_SPEEDUP" \
    '{gpu: $gpu, rounds: $rounds, min_speedup: $min}')
KEY=$(crux draft-launch "$DRAFT" --params "$PARAMS" --max-cost "$MAX_COST" --max-time "$MAX_TIME" --json \
    | jq -r .key)
log "launched $KEY with $PARAMS"

deadline=$((SECONDS + TIMEOUT_SECS))
while :; do
    crux playbook-run "$KEY" > "$WORK/launch.json"
    status=$(jq -r .status "$WORK/launch.json")
    [[ $status =~ ^(done|parked|failed|finished|cancelled)$ ]] && break
    [ $SECONDS -lt $deadline ] || fail "launch $KEY did not finish within ${TIMEOUT_SECS}s"
    sleep 30
done
RUN=$(jq -r '[.. | objects | .run_id? // empty] | first // empty' "$WORK/launch.json")
log "launch $KEY ended $status, run ${RUN:-none}"
[ -n "$RUN" ] || fail "launch $KEY started no run: $(jq -c . "$WORK/launch.json")"
crux graph "$RUN"
crux run-files "$RUN"
crux graph "$RUN" --json > "$WORK/graph.json"
log "verify: $(jq -c '[.results[] | select(.task == "verify")] | last' "$WORK/graph.json")"
jq -e '[.results[] | select(.task == "verify")] | last | .status == "pass"' "$WORK/graph.json" >/dev/null \
    || fail "the verifier did not pass run $RUN"
log "PASS"
