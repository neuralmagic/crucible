#!/usr/bin/env bash
# End-to-end proof of pack delivery against a real cluster: the controller from this checkout
# runs on the host with the pod executor, dispatches into a throwaway kind cluster, and pulls
# its loop image from a plain-HTTP registry:2 on the kind network.
#   A  a command-only draft launch completes; its pack arrives as one gzipped ConfigMap key
#      that pack-stage extracts, though the raw tar is over the ConfigMap size limit; the pod
#      and its ConfigMap are collected afterwards
#   P  the draft published to the registry launches with the same delivery
#   F  a failing task parks its launch with the task's error
#   C  a pack over the delivery budget is refused at save
#   R  runs survive a controller restart: one finishes while the controller is down, one is
#      still running when it comes back
#   B  legacy pack rows are converted to stored trees at startup: a foreign-encoded tarball
#      converts to the same tree, a symlink pack is recorded unconvertible, and the draft
#      still launches; with its legacy bytes swapped for another pack's, it still delivers the
#      stored tree
#   M  a loop image labelled with another contract version parks the launch without a pod
# Needs docker, kind, kubectl, jq, curl. Uses $DATABASE_URL and $PG_CONTAINER when set (the CI
# postgres action), else starts its own Postgres. KEEP=1 leaves everything up; ARTIFACT_DIR
# collects logs on failure.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
FIX="$ROOT/e2e/kind"
ID="${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-$$}"
CLUSTER="crucible-e2e-$ID"
REG="crucible-e2e-reg-$ID"
REGPORT="${REGPORT:-$((20000 + RANDOM % 10000))}"
PORT="${PORT:-$((30000 + RANDOM % 10000))}"
NS=crucible-e2e
DRAFT=e2e
SKOPEO=quay.io/skopeo/stable:v1.20
mkdir -p "$ROOT/target"
WORK=$(mktemp -d "$ROOT/target/kind-e2e.XXXXXX")
ARTIFACT_DIR="${ARTIFACT_DIR:-$WORK/artifacts}"
OWN_PG=""
CONTROLLER_PID=""
BG_PIDS=()

for tool in docker kind kubectl jq curl; do
    command -v "$tool" >/dev/null || { echo "missing $tool" >&2; exit 1; }
done

log() { echo "==> $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

collect() {
    mkdir -p "$ARTIFACT_DIR"
    cp "$WORK"/*.log "$WORK"/*.json "$ARTIFACT_DIR"/ 2>/dev/null || true
    kubectl get events -A --sort-by=.lastTimestamp >"$ARTIFACT_DIR/events.txt" 2>&1 || true
    kubectl -n "$NS" get all,cm -o yaml >"$ARTIFACT_DIR/objects.yaml" 2>&1 || true
    kind export logs --name "$CLUSTER" "$ARTIFACT_DIR/kind" >/dev/null 2>&1 || true
    docker logs "$REG" >"$ARTIFACT_DIR/registry.log" 2>&1 || true
    sql -c "SELECT key, status, parked_reason FROM issues" </dev/null >"$ARTIFACT_DIR/issues.txt" 2>&1 || true
    echo "artifacts: $ARTIFACT_DIR" >&2
}

cleanup() {
    local rc=$?
    if [ -n "$CONTROLLER_PID" ]; then
        kill "$CONTROLLER_PID" 2>/dev/null || true
        wait "$CONTROLLER_PID" 2>/dev/null || true
    fi
    for p in ${BG_PIDS[@]+"${BG_PIDS[@]}"}; do kill "$p" 2>/dev/null || true; done
    [ "$rc" -ne 0 ] && collect
    if [ "${KEEP:-0}" = 1 ]; then
        echo "KEEP=1: cluster $CLUSTER, registry $REG, work $WORK, controller port $PORT"
        return
    fi
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
    docker rm -f "$REG" >/dev/null 2>&1 || true
    [ -n "$OWN_PG" ] && docker rm -f "$OWN_PG" >/dev/null 2>&1
    [ "$rc" -eq 0 ] && rm -rf "$WORK"
    return 0
}
trap cleanup EXIT

sql() { docker exec -i "$PG" psql -qAt -v ON_ERROR_STOP=1 -U postgres -d kind_e2e "$@"; }

# wait_for <seconds> <description> <command...>: poll until the command succeeds.
wait_for() {
    local secs="$1" what="$2"
    shift 2
    for _ in $(seq 1 "$secs"); do
        "$@" >/dev/null 2>&1 && return 0
        sleep 1
    done
    fail "timed out after ${secs}s waiting for $what"
}

# ---- build ---------------------------------------------------------------------------------
NODE_ARCH=$(docker info -f '{{.Architecture}}')
case "$NODE_ARCH" in x86_64 | amd64) NODE_ARCH=amd64 ;; aarch64 | arm64) NODE_ARCH=arm64 ;; esac

if [ "${SKIP_BUILD:-0}" != 1 ]; then
    log "building the controller, crux, and crucible for the host"
    (cd "$ROOT" && SQLX_OFFLINE=true cargo build --locked -q -p crucible -p crucible-controller -p crux --bins)
fi
BIN="$ROOT/target/debug"

mkdir -p "$WORK/image"
HOST_ARCH=$(uname -m)
case "$HOST_ARCH" in x86_64) HOST_ARCH=amd64 ;; aarch64) HOST_ARCH=arm64 ;; esac
if [ -n "${CRUCIBLE_LINUX_BIN:-}" ]; then
    cp "$CRUCIBLE_LINUX_BIN" "$WORK/image/crucible"
    LOOP_BASE="${LOOP_BASE:-ubuntu:24.04}"
elif [ "$(uname -s)" = Linux ] && [ "$HOST_ARCH" = "$NODE_ARCH" ]; then
    cp "$BIN/crucible" "$WORK/image/crucible"
    LOOP_BASE="${LOOP_BASE:-ubuntu:24.04}"
else
    log "building crucible for linux/$NODE_ARCH in rust:1-bookworm"
    docker run --rm --platform "linux/$NODE_ARCH" \
        -v "$ROOT":/src -w /src \
        -v kind-e2e-cargo:/usr/local/cargo/registry \
        -v "kind-e2e-target-$NODE_ARCH":/ctarget \
        -v "$WORK/image":/out \
        -e CARGO_TARGET_DIR=/ctarget -e SQLX_OFFLINE=true \
        rust:1-bookworm sh -c 'cargo build --locked -q -p crucible --bin crucible && cp /ctarget/debug/crucible /out/'
    LOOP_BASE="${LOOP_BASE:-debian:bookworm-slim}"
fi
CONTRACT_VERSION=$("$BIN/crucible" --contract-version)

# ---- cluster and registry ------------------------------------------------------------------
log "creating kind cluster $CLUSTER"
kind create cluster -q --name "$CLUSTER" --config "$FIX/cluster.yaml" --wait 120s \
    --kubeconfig "$WORK/kubeconfig" >/dev/null
export KUBECONFIG="$WORK/kubeconfig"

log "starting registry $REG on localhost:$REGPORT"
docker run -d -q --name "$REG" --network kind -p "127.0.0.1:$REGPORT:5000" registry:2 >/dev/null
for node in $(kind get nodes --name "$CLUSTER"); do
    docker exec "$node" mkdir -p "/etc/containerd/certs.d/localhost:$REGPORT"
    printf '[host."http://%s:5000"]\n' "$REG" |
        docker exec -i "$node" tee "/etc/containerd/certs.d/localhost:$REGPORT/hosts.toml" >/dev/null
done

kubectl create namespace "$NS" >/dev/null
kubectl -n "$NS" create serviceaccount crucible-loop >/dev/null
kubectl -n "$NS" create configmap crucible-e2e-kubeconfig --from-literal=kubeconfig= >/dev/null

# ---- loop images ---------------------------------------------------------------------------
# push_loop_image <tag> <contract label>: build the loop image and push it to the registry.
push_loop_image() {
    docker build -q --load --platform "linux/$NODE_ARCH" \
        --build-arg "BASE=$LOOP_BASE" --build-arg "CONTRACT_VERSION=$CONTRACT_VERSION" \
        --build-arg "CONTRACT_LABEL=$2" \
        -f "$FIX/Containerfile.loop" -t "crucible-e2e-loop:$1" "$WORK/image" >/dev/null
    docker save -o "$WORK/image/loop.tar" "crucible-e2e-loop:$1"
    docker run --rm -q --network kind -v "$WORK/image":/w "$SKOPEO" copy -q --dest-tls-verify=false \
        "docker-archive:/w/loop.tar" "docker://$REG:5000/crucible-e2e-loop:$1"
    rm "$WORK/image/loop.tar"
    docker rmi "crucible-e2e-loop:$1" >/dev/null 2>&1 || true
}
LOOP_IMAGE="localhost:$REGPORT/crucible-e2e-loop:$ID"
MISMATCH_IMAGE="localhost:$REGPORT/crucible-e2e-loop:$ID-mismatch"
log "building loop images on $LOOP_BASE (contract $CONTRACT_VERSION)"
push_loop_image "$ID" "$CONTRACT_VERSION"
push_loop_image "$ID-mismatch" 0.0.0
sed "s|@LOOP_IMAGE@|$LOOP_IMAGE|g" "$FIX/profile.toml.in" >"$WORK/profile.toml"
sed "s|@LOOP_IMAGE@|$MISMATCH_IMAGE|g" "$FIX/profile.toml.in" >"$WORK/profile-mismatch.toml"

# ---- postgres ------------------------------------------------------------------------------
if [ -n "${PG_CONTAINER:-}" ]; then
    PG="$PG_CONTAINER"
    PG_PORT=$(docker port "$PG" 5432/tcp | head -n1 | cut -d: -f2)
else
    PG="crucible-e2e-pg-$ID"
    OWN_PG="$PG"
    docker run -d -q --name "$PG" -e POSTGRES_PASSWORD=ci -e POSTGRES_DB=crucible \
        --tmpfs /var/lib/postgresql/data:rw -p 127.0.0.1::5432 postgres:16 >/dev/null
    wait_for 60 postgres docker exec "$PG" pg_isready -h 127.0.0.1 -U postgres -d crucible
    PG_PORT=$(docker port "$PG" 5432/tcp | head -n1 | cut -d: -f2)
fi
docker exec "$PG" psql -qAt -U postgres -d crucible -c 'DROP DATABASE IF EXISTS kind_e2e' -c 'CREATE DATABASE kind_e2e'
DB="postgres://postgres:ci@127.0.0.1:$PG_PORT/kind_e2e"

# ---- controller ----------------------------------------------------------------------------
export CONTROLLER_URL="http://127.0.0.1:$PORT" CONTROLLER_CONFIG=/dev/null
unset CONTROLLER_API_TOKEN

BOOT=0
# start_controller [profile]: boot the controller and wait for it to answer.
start_controller() {
    BOOT=$((BOOT + 1))
    log "starting controller (boot $BOOT) on $CONTROLLER_URL"
    env -u CONTROLLER_PROXY_TOKEN -u CONTROLLER_OIDC_ISSUER -u VAULT_ADDR \
        -u POD_NAME -u POD_NAMESPACE \
        DATABASE_URL="$DB" \
        CONTROLLER_API_ADDR="127.0.0.1:$PORT" CONTROLLER_PUBLIC_URL="$CONTROLLER_URL" \
        CONTROLLER_DEV_IDENTITY=e2e CONTROLLER_ADMINS=e2e CONTROLLER_AUTH_MODE=proxy \
        CONTROLLER_SESSION_SECURE=false \
        CONTROLLER_PLAYBOOK_EXECUTOR=pod CONTROLLER_SCOPE_EXECUTOR=disabled \
        CONTROLLER_DEPLOY_PROFILE="${1:-$WORK/profile.toml}" CONTROLLER_POD_NAMESPACE="$NS" \
        CONTROLLER_TURN_SERVICE_ACCOUNT=crucible-loop \
        CONTROLLER_STATE_DIR="$WORK/state" CONTROLLER_SCRATCH_DIR="$WORK/scratch" \
        CRUCIBLE_BIN="$BIN/crucible" FORGE_INSECURE_REGISTRIES="localhost:$REGPORT" \
        RUST_LOG="${RUST_LOG:-info}" NO_COLOR=1 \
        "$BIN/crucible-controller" autopilot >"$WORK/controller-$BOOT.log" 2>&1 &
    CONTROLLER_PID=$!
    wait_for 90 "controller health" curl -sf "$CONTROLLER_URL/healthz"
}

stop_controller() {
    kill "$CONTROLLER_PID"
    wait "$CONTROLLER_PID" 2>/dev/null || true
    CONTROLLER_PID=""
}

crux() { "$BIN/crux" "$@"; }

# conversion_report <boot>: the converted/unconvertible counts the boot logged, "none" if silent.
conversion_report() {
    local line
    line=$(grep 'legacy packs converted to stored trees' "$WORK/controller-$1.log" || true)
    [ -z "$line" ] && { echo none; return; }
    echo "$(sed -E 's/.*converted=([0-9]+).*/\1/' <<<"$line") $(sed -E 's/.*unconvertible=([0-9]+).*/\1/' <<<"$line")"
}

# ---- pack fixtures -------------------------------------------------------------------------
PACK="$WORK/deliver"
cp -R "$FIX/packs/deliver" "$PACK"
mkdir -p "$PACK/bulk"
for f in a b c; do head -c 512000 <(yes "pack delivery filler $f") >"$PACK/bulk/$f.txt"; done

# small_pack <dir> <check.sh body>: a one-task pack whose task runs the given shell.
small_pack() {
    mkdir -p "$1"
    cp "$FIX/packs/deliver/workflow.star" "$1/"
    sed 's/^inject = .*/inject = ["check.sh"]/' "$FIX/packs/deliver/crucible.toml" >"$1/crucible.toml"
    printf '#!/bin/sh\nset -eu\n%s\n' "$2" >"$1/check.sh"
}

# new_draft <id> <dir>: create a draft and save the directory as its version 2.
new_draft() {
    crux draft-create "$1" --description "kind e2e" >/dev/null
    crux draft-push "$1" "$2" --base-version 1 --json >"$WORK/push-$1.json"
    [ "$(jq -r .version "$WORK/push-$1.json")" = 2 ] || fail "draft-push $1: $(cat "$WORK/push-$1.json")"
}

# launch <label> <crux launch command...>: start a launch; its key lands in KEY.
launch() {
    local label="$1"
    shift
    KEY=$(crux "$@" --max-cost 1 --max-time 5m --json | tee "$WORK/launch-$label.json" | jq -r .key)
    [ -n "$KEY" ] && [ "$KEY" != null ] || fail "[$label] $1 returned no key"
    log "[$label] launched $KEY"
}

# settle <label> <key>: wait for the launch to stop moving; its status lands in STATUS and the
# whole launch in $WORK/run-<label>.json.
settle() {
    STATUS=""
    for _ in $(seq 1 300); do
        crux playbook-run "$2" >"$WORK/run-$1.json" 2>/dev/null || true
        STATUS=$(jq -r '.launch.status // empty' "$WORK/run-$1.json" 2>/dev/null || true)
        case "$STATUS" in done | pr-open | parked) return ;; esac
        sleep 1
    done
    fail "[$1] launch still '$STATUS' after 300s"
}

# expect_finished <label>: the settled launch is done and its run finished.
expect_finished() {
    [ "$STATUS" = "done" ] || fail "[$1] launch ended '$STATUS': $(jq -c '.launch' "$WORK/run-$1.json")"
    [ "$(jq -r '.runs[0].status' "$WORK/run-$1.json")" = finished ] ||
        fail "[$1] run did not finish: $(jq -c '.runs[0]' "$WORK/run-$1.json")"
    pass "[$1] launch done, run finished"
}

# expect_parked <label> <reason fragment>: the settled launch parked for the given reason.
expect_parked() {
    local reason
    reason=$(jq -r '.launch.parked_reason // empty' "$WORK/run-$1.json")
    if [ "$STATUS" != parked ] || ! grep -qF "$2" <<<"$reason"; then
        fail "[$1] expected a park naming '$2', got '$STATUS': $reason"
    fi
    pass "[$1] parked: $reason"
}

watch_start() {
    kubectl -n "$NS" get pods -w --output-watch-events -o json >"$WORK/pods-$1.json" 2>/dev/null &
    BG_PIDS+=($!)
    kubectl -n "$NS" get configmaps -w --output-watch-events -o json >"$WORK/cms-$1.json" 2>/dev/null &
    BG_PIDS+=($!)
}

watch_stop() {
    for p in ${BG_PIDS[@]+"${BG_PIDS[@]}"}; do kill "$p" 2>/dev/null || true; done
    BG_PIDS=()
}

# pods_in <jq predicate> <count>: exactly count run pods in the namespace match the predicate.
pods_in() {
    [ "$(kubectl -n "$NS" get pods -o json | jq "[.items[] | select($1)] | length")" = "$2" ]
}

# no_run_objects: no pods and no pack ConfigMaps are left in the namespace.
no_run_objects() {
    pods_in true 0 &&
        [ "$(kubectl -n "$NS" get configmaps -o json | jq '[.items[] | select(.binaryData["pack.tar.gz"])] | length')" = 0 ]
}

# check_delivery <label>: assert, from the watch streams, how the pack reached the pod.
check_delivery() {
    local label="$1" pod cm gz raw listing
    pod=$(jq -rs '[.[] | .object | select(.spec.initContainers[]?.name == "pack-stage")] | last' "$WORK/pods-$label.json")
    [ "$pod" != null ] || fail "[$label] no pod with a pack-stage init container was seen"
    [ "$(jq -r '[.status.initContainerStatuses[]? | select(.name == "pack-stage") | .state.terminated.exitCode] | first' <<<"$pod")" = 0 ] ||
        fail "[$label] pack-stage did not exit 0: $(jq -c '.status.initContainerStatuses' <<<"$pod")"
    jq -r '.spec.initContainers[] | select(.name == "pack-stage") | .args | join(" ")' <<<"$pod" |
        grep -q 'tar -xzf /opt/crucible/pack-src/pack.tar.gz' || fail "[$label] pack-stage does not extract pack.tar.gz"
    pass "[$label] pack-stage extracted the tarball"

    cm=$(jq -rs '[.[] | .object | select(.binaryData["pack.tar.gz"] != null)] | last' "$WORK/cms-$label.json")
    [ "$cm" != null ] || fail "[$label] no pack ConfigMap was seen"
    [ "$(jq -r '(.binaryData | keys | join(",")) + " " + ((.data // {}) | length | tostring) + " " + (.immutable | tostring)' <<<"$cm")" = "pack.tar.gz 0 true" ] ||
        fail "[$label] pack ConfigMap is not one immutable binary key: $(jq -c '{binaryData: (.binaryData | keys), data, immutable}' <<<"$cm")"
    [ "$(jq -r '.metadata.ownerReferences[0].uid' <<<"$cm")" = "$(jq -r '.metadata.uid' <<<"$pod")" ] ||
        fail "[$label] pack ConfigMap is not owned by its pod"
    jq -r '.binaryData["pack.tar.gz"]' <<<"$cm" | base64 -d >"$WORK/delivered-$label.tar.gz"
    gz=$(wc -c <"$WORK/delivered-$label.tar.gz" | tr -d ' ')
    raw=$(gzip -dc "$WORK/delivered-$label.tar.gz" | wc -c | tr -d ' ')
    [ "$gz" -le 921600 ] || fail "[$label] delivered tarball is $gz bytes, over the budget"
    [ "$raw" -gt 1048576 ] || fail "[$label] raw tar is only $raw bytes; the fixture no longer exceeds the ConfigMap limit"
    listing=$(tar -tzf "$WORK/delivered-$label.tar.gz" | sort | tr '\n' ' ')
    [ "$listing" = "bulk/a.txt bulk/b.txt bulk/c.txt check.sh crucible.toml nested/deep/marker.txt workflow.star " ] ||
        fail "[$label] delivered tarball lists: $listing"
    pass "[$label] pack delivered as one gzipped key ($gz bytes gzipped, $raw raw)"
}

# launch_and_check <label> <crux launch command...>: launch, expect success, check delivery.
launch_and_check() {
    local label="$1"
    shift
    watch_start "$label"
    launch "$label" "$@"
    settle "$label" "$KEY"
    expect_finished "$label"
    check_delivery "$label"
    watch_stop
}

# ---- scenario A ----------------------------------------------------------------------------
start_controller
[ "$(crux whoami 2>/dev/null | head -n1)" != "" ] || fail "crux cannot reach the controller"

log "[A] creating draft $DRAFT and saving the delivery pack"
new_draft "$DRAFT" "$PACK"
VERSION=2
launch_and_check A draft-launch "$DRAFT"
wait_for 90 "the run's pod and pack ConfigMap to be collected" no_run_objects
pass "[A] pod and pack ConfigMap collected"

# ---- scenario P ----------------------------------------------------------------------------
log "[P] publishing $DRAFT as playbook $DRAFT-pub"
crux draft-publish "$DRAFT" --playbook "$DRAFT-pub" --json >"$WORK/publish-P.json"
[ "$(jq -r .id "$WORK/publish-P.json")" = "$DRAFT-pub" ] || fail "[P] publish: $(cat "$WORK/publish-P.json")"
launch_and_check P launch "$DRAFT-pub"

# ---- scenario F ----------------------------------------------------------------------------
small_pack "$WORK/fail" 'echo "deliberate failure" >&2
exit 1'
new_draft "$DRAFT-fail" "$WORK/fail"
launch F draft-launch "$DRAFT-fail"
settle F "$KEY"
expect_parked F "deliberate failure"
[ "$(jq -r '.runs[0].status' "$WORK/run-F.json")" = error ] ||
    fail "[F] run status is $(jq -c '.runs[0].status' "$WORK/run-F.json")"

# ---- scenario C ----------------------------------------------------------------------------
log "[C] saving a pack over the delivery budget"
OVER="$WORK/overbudget"
cp -R "$FIX/packs/deliver" "$OVER"
for f in 1 2 3; do head -c 337500 /dev/urandom | base64 | tr -d '\n' >"$OVER/blob$f.txt"; done
if crux draft-push "$DRAFT" "$OVER" --base-version "$VERSION" >"$WORK/push-C.log" 2>&1; then
    fail "[C] an over-budget pack was saved"
fi
grep -q 'delivery budget' "$WORK/push-C.log" || fail "[C] refusal does not name the budget: $(cat "$WORK/push-C.log")"
[ "$(sql -c "SELECT max(version) FROM playbook_draft_versions WHERE draft_id = '$DRAFT'")" = "$VERSION" ] ||
    fail "[C] the refused save left a version behind"
pass "[C] over-budget save refused"

# ---- scenario R ----------------------------------------------------------------------------
small_pack "$WORK/short" "sleep 10
echo '{\"ok\":true}'"
small_pack "$WORK/long" "sleep 45
echo '{\"ok\":true}'"
new_draft "$DRAFT-short" "$WORK/short"
new_draft "$DRAFT-long" "$WORK/long"
launch R-short draft-launch "$DRAFT-short"
SHORT="$KEY"
launch R-long draft-launch "$DRAFT-long"
LONG="$KEY"
wait_for 120 "both run pods to be running" pods_in '.status.containerStatuses[]?.state.running' 2
SHORT_RUN=$(crux playbook-run "$SHORT" | jq -r '.runs[0].run_id')
LONG_RUN=$(crux playbook-run "$LONG" | jq -r '.runs[0].run_id')
stop_controller
log "[R] controller down; waiting for the short run to finish"
wait_for 60 "the short run's pod to succeed" pods_in '.status.phase == "Succeeded"' 1
pods_in '.status.containerStatuses[]?.state.running' 1 || fail "[R] the long run finished before the controller came back"
start_controller
for run in "R-short $SHORT $SHORT_RUN" "R-long $LONG $LONG_RUN"; do
    read -r label key run_id <<<"$run"
    settle "$label" "$key"
    expect_finished "$label"
    [ "$(jq -r '[.runs[].run_id] | join(" ")' "$WORK/run-$label.json")" = "$run_id" ] ||
        fail "[$label] expected only run $run_id, got $(jq -c '[.runs[].run_id]' "$WORK/run-$label.json")"
done
pass "[R] both runs were collected, not re-dispatched"

# ---- scenario B ----------------------------------------------------------------------------
stop_controller
log "[B] seeding a symlink pack that cannot become a tree"
mkdir -p "$WORK/symlink"
echo m >"$WORK/symlink/crucible.toml"
ln -s crucible.toml "$WORK/symlink/link"
SYMLINK_HEX=$(COPYFILE_DISABLE=1 tar -C "$WORK/symlink" -czf - crucible.toml link | od -An -v -tx1 | tr -d ' \n')
sql -v hex="$SYMLINK_HEX" <<'SQL'
INSERT INTO pack_tarballs (issue_slug, tar_gz, digest, bytes, created_at)
SELECT 'kind_e2e_symlink', b, 'sha256:' || encode(sha256(b), 'hex'), length(b), now()::TEXT
FROM (SELECT decode(:'hex', 'hex') AS b) s;
SQL

start_controller
read -r converted unconvertible <<<"$(conversion_report "$BOOT")"
[ "$unconvertible" = 1 ] || fail "[B] boot $BOOT reported '$converted $unconvertible'; expected one unconvertible pack"
[ "$(sql -c "SELECT count(*) FROM playbook_draft_versions WHERE tree_digest IS NULL")" = 0 ] ||
    fail "[B] draft versions are left unconverted"
[ "$(sql -c "SELECT count(*) FROM pack_digest_aliases WHERE unconvertible_reason IS NOT NULL")" = 1 ] ||
    fail "[B] the symlink pack has no unconvertible alias"
TREE=$(sql -c "SELECT tree_digest FROM playbook_draft_versions WHERE draft_id = '$DRAFT' AND version = $VERSION")
[[ "$TREE" =~ ^tree1:[0-9a-f]{64}$ ]] || fail "[B] version $VERSION has tree digest '$TREE'"
sql -c "SELECT path FROM pack_tree_files WHERE digest = '$TREE'" | grep -qx 'nested/deep/marker.txt' ||
    fail "[B] the stored tree is missing nested/deep/marker.txt"
pass "[B] every pack row holds a tree, and the symlink pack is recorded unconvertible"

stop_controller
start_controller
[ "$(conversion_report "$BOOT")" = none ] || fail "[B] boot $BOOT converted again: $(conversion_report "$BOOT")"
pass "[B] a restart with nothing to convert stays silent"

stop_controller
log "[B] rewriting version $VERSION with a GNU tar encoding of the same tree"
FOREIGN_HEX=$(COPYFILE_DISABLE=1 tar -C "$PACK" -czf - . | od -An -v -tx1 | tr -d ' \n')
sql -v hex="$FOREIGN_HEX" -v draft="$DRAFT" -v version="$VERSION" <<'SQL'
UPDATE playbook_draft_versions
SET tar_gz = s.b, tar_digest = 'sha256:' || encode(sha256(s.b), 'hex'), tar_bytes = length(s.b)
FROM (SELECT decode(:'hex', 'hex') AS b) s
WHERE draft_id = :'draft' AND version = :'version';
SQL
[ -z "$(sql -c "SELECT tree_digest FROM playbook_draft_versions WHERE draft_id = '$DRAFT' AND version = $VERSION")" ] ||
    fail "[B] rewriting the legacy bytes alone did not clear the tree digest"

start_controller
read -r converted unconvertible <<<"$(conversion_report "$BOOT")"
[ "$converted $unconvertible" = "1 0" ] ||
    fail "[B] boot $BOOT reported '$converted $unconvertible'; expected only the rewritten version"
RETREE=$(sql -c "SELECT tree_digest FROM playbook_draft_versions WHERE draft_id = '$DRAFT' AND version = $VERSION")
[ "$RETREE" = "$TREE" ] || fail "[B] the GNU tar encoding converted to a different tree:
$(diff <(sql -c "SELECT path, sha256 FROM pack_tree_files WHERE digest = '$TREE' ORDER BY path") \
    <(sql -c "SELECT path, sha256 FROM pack_tree_files WHERE digest = '$RETREE' ORDER BY path"))"
pass "[B] a foreign encoding of the same files converted to the same tree"
launch_and_check B draft-launch "$DRAFT"

log "[B] pointing version $VERSION's legacy bytes at a different pack, keeping its tree"
small_pack "$WORK/decoy" 'echo decoy'
DECOY_HEX=$(COPYFILE_DISABLE=1 tar -C "$WORK/decoy" -czf - . | od -An -v -tx1 | tr -d ' \n')
sql -v hex="$DECOY_HEX" -v draft="$DRAFT" -v version="$VERSION" <<'SQL'
BEGIN;
ALTER TABLE playbook_draft_versions DISABLE TRIGGER playbook_draft_versions_legacy_bytes;
UPDATE playbook_draft_versions
SET tar_gz = s.b, tar_digest = 'sha256:' || encode(sha256(s.b), 'hex'), tar_bytes = length(s.b)
FROM (SELECT decode(:'hex', 'hex') AS b) s
WHERE draft_id = :'draft' AND version = :'version';
ALTER TABLE playbook_draft_versions ENABLE TRIGGER playbook_draft_versions_legacy_bytes;
COMMIT;
SQL
DECOY_DIGEST=$(sql -c "SELECT tar_digest FROM playbook_draft_versions WHERE draft_id = '$DRAFT' AND version = $VERSION")
[ "$(sql -c "SELECT tree_digest FROM playbook_draft_versions WHERE draft_id = '$DRAFT' AND version = $VERSION")" = "$TREE" ] ||
    fail "[B] version $VERSION lost its tree when its bytes were rewritten"
launch_and_check B-tree draft-launch "$DRAFT"
[ "$(sql -c "SELECT count(*) FROM pack_tarballs WHERE tree_digest = '$TREE' AND digest = '$DECOY_DIGEST'")" = 1 ] ||
    fail "[B] the launch did not carry the decoy bytes beside the tree"
pass "[B] dispatch delivered the stored tree, not the launch's legacy bytes"


# ---- scenario M ----------------------------------------------------------------------------
stop_controller
start_controller "$WORK/profile-mismatch.toml"
curl -sf "$CONTROLLER_URL/api/config" >"$WORK/config-M.json"
[ "$(jq -r --arg ref "$MISMATCH_IMAGE" '.contract.images[] | select(.reference == $ref) | "\(.engine_version) \(.match)"' "$WORK/config-M.json")" = "0.0.0 false" ] ||
    fail "[M] /api/config does not report the mismatch: $(jq -c .contract "$WORK/config-M.json")"
watch_start M
launch M launch "$DRAFT-pub"
settle M "$KEY"
watch_stop
expect_parked M "contract rejection"
[ "$(jq -s --arg key "${KEY//:/-}" '[.[] | select(.object.metadata.labels["crucible.dev/issue-key"] == $key)] | length' "$WORK/pods-M.json")" = 0 ] ||
    fail "[M] a pod was created for a rejected launch"
pass "[M] no pod was created"

echo "kind e2e passed"
