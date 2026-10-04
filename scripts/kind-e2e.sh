#!/usr/bin/env bash
# End-to-end proof of pack delivery against a real cluster: the controller from this checkout
# runs on the host with the pod executor, dispatches into a throwaway kind cluster, and pulls
# its loop image from a plain-HTTP registry:2 on the kind network.
#   A  a command-only draft launch completes; its pack arrives as one gzipped ConfigMap key
#      that pack-stage extracts, though the raw tar is over the ConfigMap size limit; the pod
#      and its ConfigMap are collected afterwards; the save recorded the tree the launch runs and
#      the tarball download names it in X-Pack-Digest; the API and its OpenAPI spec name the digest
#      tree_digest; crux's push and pull skip state/, workspace/ and .git at any depth, refuse a
#      symlink, and print the local tree and the skipped paths
#   P  the draft published to the registry launches with the same delivery; the registry row and
#      its revision hold the draft's tree, the launch records the registry row's exposure, and the
#      registry inspector and a template clone read that tree
#   F  a failing task parks its launch with the task's error
#   C  a pack over the delivery budget is refused at save
#   R  runs survive a controller restart: one finishes while the controller is down, one is
#      still running when it comes back
#   B  legacy pack rows are converted to stored trees at startup: a foreign-encoded tarball
#      converts to the same tree, a symlink pack and an archive past the expansion bound are
#      recorded unconvertible, and the draft still launches; with its legacy bytes swapped for
#      another pack's, it still delivers the stored tree
#   D  the download, the draft's files, and launch adoption read the stored tree: a save or a
#      delete that lands while a launch waits on the version row refuses the launch
#   L  a version whose bytes change on a live controller loses its tree; it is read, downloaded,
#      and delivered from its legacy bytes, and converts to the downloaded tree at the next boot; a
#      registry row whose bytes change is inspected, templated, and launched from them alike
#   T  a stored tree whose files were tampered with refuses its draft launch and parks its
#      registered launch without a pod; the startup agent backfill skips it and fills the rest
#   E  a restart re-derives a stale registry row from its tree, not its bytes
#   I  a git pack is proposed, compiled, registered, opened as a draft, and registered directly,
#      each holding the same tree with state/ ignored, named tree_digest through the API and crux;
#      an over-budget repo is refused
#   W  a webhook adopted from a converted foreign encoding fires its adopted tree after the
#      playbook is re-registered, keeps the encoding's bytes, and records and serves that tree's
#      exposure; a one-shot against the new revision runs the new tree
#   S  steering rows reach the run as STEER.md in a per-run inputs key beside the untouched pack,
#      count against the delivery budget, and satisfy an inject the pack does not ship
#   K  a schedule's cursor file reaches the run in the inputs key, at the pack root or under
#      state/ (also over a state PVC), and a draft-head schedule fires the newest compiled tree,
#      not a newer save that does not compile
#   X  a registry row re-registered or deleted between authorization and save refuses a launch,
#      schedule, schedule edit, webhook, or one-shot with 409 or 404, and stores nothing
#   Q  an approved direct autoresearch pack plans and delivers the tree its scope froze, not one
#      written to its pack row afterwards, and its approval evidence reads SCOPE.md from the
#      stored tree (needs the controller built with the autoresearch lane)
#   O  the local executor runs a stored tree with its steering beside it, and parks a tampered tree
#   V  conversion makes a draft-sourced rev its tree and pins a secret binding and a fork to it, so
#      neither is stale or moved across restarts and an identical republish, while new content
#      stales and moves them; a launch is checked against the tree it froze; a pre-tree digest is
#      refused naming its replacement only to a reader of a playbook that held it; a same-rev
#      tree change makes a webhook edit a new firing; unconvertible packs are refused naming the
#      reason at draft launch, template clone, open-as-draft, standing fire, and dispatch; an
#      unpinned old tree is collected at startup and its legacy bytes store it again; the engine's
#      scope pack is the controller's canonical tarball of the walked tree
#   G  migrate-state stores a legacy pack dir as a tree with its steering split into rows
#   M  a loop image labelled with another contract version parks the launch without a pod
#   Z  db rebuild carries the trees and aliases the pack rows reference
# Needs docker, kind, kubectl, jq, curl, git, shasum. Uses $DATABASE_URL and $PG_CONTAINER when
# set (the CI postgres action), else starts its own Postgres. KEEP=1 leaves everything up;
# ARTIFACT_DIR collects logs on failure.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
FIX="$ROOT/e2e/kind"
ID="${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-$$}"
CLUSTER="crucible-e2e-$ID"
REG="crucible-e2e-reg-$ID"
REGPORT="${REGPORT:-$((20000 + RANDOM % 10000))}"
PORT="${PORT:-$((30000 + RANDOM % 10000))}"
HOOKPORT="${HOOKPORT:-$((40000 + RANDOM % 10000))}"
NS=crucible-e2e
DRAFT=e2e
SKOPEO=quay.io/skopeo/stable:v1.20
mkdir -p "$ROOT/target"
WORK=$(mktemp -d "$ROOT/target/kind-e2e.XXXXXX")
ARTIFACT_DIR="${ARTIFACT_DIR:-$WORK/artifacts}"
OWN_PG=""
PG=""
CONTROLLER_PID=""
CONTROLLER_ENV=()
BG_PIDS=()
API_PIDS=()

for tool in docker kind kubectl jq curl git shasum; do
    command -v "$tool" >/dev/null || { echo "missing $tool" >&2; exit 1; }
done

log() { echo "==> $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

collect() {
    mkdir -p "$ARTIFACT_DIR"
    cp "$WORK"/*.log "$WORK"/*.json "$ARTIFACT_DIR"/ 2>/dev/null || true
    kubectl get events -A --sort-by=.lastTimestamp >"$ARTIFACT_DIR/events.txt" 2>&1 || true
    kubectl -n "$NS" get all,cm,pvc -o yaml >"$ARTIFACT_DIR/objects.yaml" 2>&1 || true
    kind export logs --name "$CLUSTER" "$ARTIFACT_DIR/kind" >/dev/null 2>&1 || true
    docker logs "$REG" >"$ARTIFACT_DIR/registry.log" 2>&1 || true
    if [ -n "$PG" ]; then
        sql -c "SELECT key, status, parked_reason FROM issues" </dev/null >"$ARTIFACT_DIR/issues.txt" 2>&1 || true
    fi
    echo "artifacts: $ARTIFACT_DIR" >&2
}

cleanup() {
    local rc=$?
    if [ -n "$CONTROLLER_PID" ]; then
        kill "$CONTROLLER_PID" 2>/dev/null || true
        wait "$CONTROLLER_PID" 2>/dev/null || true
    fi
    for p in ${BG_PIDS[@]+"${BG_PIDS[@]}"} ${API_PIDS[@]+"${API_PIDS[@]}"}; do kill "$p" 2>/dev/null || true; done
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
    log "building the controller and the engine with the autoresearch lane"
    (cd "$ROOT" && SQLX_OFFLINE=true cargo build --locked -q -p crucible-controller --features autoresearch \
        --bin crucible-controller --target-dir "$ROOT/target/autoresearch")
    (cd "$ROOT" && SQLX_OFFLINE=true cargo build --locked -q -p crucible --features autoresearch \
        --bin crucible --target-dir "$ROOT/target/autoresearch")
fi
BIN="$ROOT/target/debug"
AUTORESEARCH_CONTROLLER="$ROOT/target/autoresearch/debug/crucible-controller"
AUTORESEARCH_ENGINE="$ROOT/target/autoresearch/debug/crucible"
CONTROLLER_BIN="$BIN/crucible-controller"

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
        -e CARGO_TARGET_DIR=/ctarget -e SQLX_OFFLINE=true -e CARGO_PROFILE_DEV_DEBUG=0 \
        rust:1-bookworm sh -c 'cargo build --locked -q -p crucible --bin crucible && cp /ctarget/debug/crucible /out/ && strip /out/crucible'
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
awk '{ print } /^\[cluster\]$/ { print "state_pvc = \"crucible-e2e-state\"" }' "$WORK/profile.toml" >"$WORK/profile-state.toml"

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
HOOKS_URL="http://127.0.0.1:$HOOKPORT"
unset CONTROLLER_API_TOKEN

BOOT=0
# start_controller [profile]: boot CONTROLLER_BIN and wait for it to answer. CONTROLLER_ENV holds
# extra VAR=value settings for this boot.
start_controller() {
    BOOT=$((BOOT + 1))
    log "starting controller (boot $BOOT) on $CONTROLLER_URL ${CONTROLLER_ENV[*]+${CONTROLLER_ENV[*]}}"
    env -u CONTROLLER_PROXY_TOKEN -u CONTROLLER_OIDC_ISSUER -u VAULT_ADDR \
        -u POD_NAME -u POD_NAMESPACE \
        DATABASE_URL="$DB" \
        CONTROLLER_API_ADDR="127.0.0.1:$PORT" CONTROLLER_PUBLIC_URL="$CONTROLLER_URL" \
        CONTROLLER_HOOKS_ADDR="127.0.0.1:$HOOKPORT" CONTROLLER_DISCOVERY_CADENCE_SECS=5 \
        CONTROLLER_DEV_IDENTITY=e2e CONTROLLER_ADMINS=e2e CONTROLLER_AUTH_MODE=proxy \
        CONTROLLER_SESSION_SECURE=false \
        CONTROLLER_PLAYBOOK_EXECUTOR=pod CONTROLLER_SCOPE_EXECUTOR=disabled \
        CONTROLLER_DEPLOY_PROFILE="${1:-$WORK/profile.toml}" CONTROLLER_POD_NAMESPACE="$NS" \
        CONTROLLER_TURN_SERVICE_ACCOUNT=crucible-loop \
        CONTROLLER_STATE_DIR="$WORK/state" CONTROLLER_SCRATCH_DIR="$WORK/scratch" \
        CRUCIBLE_BIN="$BIN/crucible" FORGE_INSECURE_REGISTRIES="localhost:$REGPORT" \
        RUST_LOG="${RUST_LOG:-info}" NO_COLOR=1 \
        ${CONTROLLER_ENV[@]+"${CONTROLLER_ENV[@]}"} \
        "$CONTROLLER_BIN" autopilot >"$WORK/controller-$BOOT.log" 2>&1 &
    CONTROLLER_PID=$!
    wait_for 90 "controller health" curl -sf "$CONTROLLER_URL/healthz"
}

stop_controller() {
    kill "$CONTROLLER_PID"
    wait "$CONTROLLER_PID" 2>/dev/null || true
    CONTROLLER_PID=""
}

crux() { "$BIN/crux" "$@"; }

# api_call <out> <method> <path> [json body]: call the controller API, writing the response body
# to <out> and printing the status. AS_USER, when set, is asserted as the caller's login.
api_call() {
    local out="$1" method="$2" path="$3" data=() who=()
    [ $# -ge 4 ] && data=(--data "$4")
    [ -n "${AS_USER:-}" ] && who=(-H "x-auth-request-user: $AS_USER")
    curl -s -o "$out" -w '%{http_code}' -X "$method" -H 'content-type: application/json' \
        ${who[@]+"${who[@]}"} ${data[@]+"${data[@]}"} "$CONTROLLER_URL$path"
}

# api <method> <path> [json body]: the status lands in HTTP and the body in $WORK/api.json.
api() { HTTP=$(api_call "$WORK/api.json" "$@"); }

# as <login> <method> <path> [json body]: api as that login. Only a boot without
# CONTROLLER_DEV_IDENTITY takes the asserted login.
as() {
    local AS_USER="$1"
    shift
    api "$@"
}

# expect_tree_digest <label> <jq path> <tree>: the object at the path in the last api body names
# the tree as tree_digest and carries no tar_digest.
expect_tree_digest() {
    local got
    got=$(jq -c "$2 | [.tree_digest, has(\"tar_digest\")]" "$WORK/api.json")
    [ "$got" = "[\"$3\",false]" ] || fail "[$1] $2 holds [tree_digest, has tar_digest] = $got, not $3 alone"
}

# expect_http <label> <status> <body fragment>: the last api call answered so.
expect_http() {
    if [ "$HTTP" != "$2" ] || ! grep -qF -- "$3" "$WORK/api.json"; then
        fail "[$1] expected HTTP $2 naming '$3', got $HTTP: $(cat "$WORK/api.json")"
    fi
}

# api_bg <name> <method> <path> [json body]: api_call in the background into $WORK/api-<name>.*;
# api_wait waits for every one started.
api_bg() {
    local name="$1"
    shift
    api_call "$WORK/api-$name.json" "$@" >"$WORK/api-$name.code" &
    API_PIDS+=($!)
}

api_wait() {
    for p in ${API_PIDS[@]+"${API_PIDS[@]}"}; do wait "$p" || true; done
    API_PIDS=()
}

# expect_bg <label> <name> <status> <body fragment>: the background call <name> answered so.
expect_bg() {
    if [ "$(cat "$WORK/api-$2.code")" != "$3" ] || ! grep -qF -- "$4" "$WORK/api-$2.json"; then
        fail "[$1] expected HTTP $3 naming '$4', got $(cat "$WORK/api-$2.code"): $(cat "$WORK/api-$2.json")"
    fi
}

# hold_lock <statement>: run the statement in a transaction left open until release_lock, so the
# row locks it takes stay held.
hold_lock() {
    rm -f "$WORK/lock.fifo"
    mkfifo "$WORK/lock.fifo"
    docker exec -i "$PG" psql -qAt -v ON_ERROR_STOP=1 -U postgres -d kind_e2e \
        <"$WORK/lock.fifo" >>"$WORK/lock.log" 2>&1 &
    LOCK_PID=$!
    exec 3>"$WORK/lock.fifo"
    printf 'BEGIN;\n%s;\n' "$1" >&3
    wait_for 30 "the lock to be held" lock_held
}

release_lock() {
    printf 'COMMIT;\n' >&3
    exec 3>&-
    wait "$LOCK_PID" || fail "the lock transaction failed: $(cat "$WORK/lock.log")"
}

lock_held() {
    [ "$(sql -c "SELECT count(*) FROM pg_stat_activity WHERE datname = 'kind_e2e' AND application_name = 'psql' AND state = 'idle in transaction'")" = 1 ]
}

# lock_waiters <n>: exactly n sessions are waiting on a lock.
lock_waiters() {
    [ "$(sql -c "SELECT count(*) FROM pg_stat_activity WHERE datname = 'kind_e2e' AND wait_event_type = 'Lock'")" = "$1" ]
}

# conversion_report <boot>: the converted/unconvertible counts the boot logged, "none" if silent.
conversion_report() {
    local line
    line=$(grep 'legacy packs converted to stored trees' "$WORK/controller-$1.log" || true)
    [ -z "$line" ] && { echo none; return; }
    echo "$(sed -E 's/.*converted=([0-9]+).*/\1/' <<<"$line") $(sed -E 's/.*unconvertible=([0-9]+).*/\1/' <<<"$line")"
}

# pinned_report <boot>: how many pins the boot's conversion derived, "none" if silent.
pinned_report() {
    local line
    line=$(grep 'legacy packs converted to stored trees' "$WORK/controller-$1.log" || true)
    [ -z "$line" ] && { echo none; return; }
    sed -E 's/.*pinned=([0-9]+).*/\1/' <<<"$line"
}

# collected_report <boot>: how many unpinned trees the boot collected, "none" if silent.
collected_report() {
    local line
    line=$(grep 'unpinned pack trees collected' "$WORK/controller-$1.log" || true)
    [ -z "$line" ] && { echo none; return; }
    sed -E 's/.*count=([0-9]+).*/\1/' <<<"$line"
}

# ---- pack and tree helpers -----------------------------------------------------------------
sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }

# slug <key>: the pack row key a launch key is stored under.
slug() {
    local s="${1//\//_}"
    s="${s//#/_}"
    echo "${s//:/_}"
}

# label <key>: the issue-key label a launch's pod carries.
label() {
    local v="${1//[!A-Za-z0-9_.-]/-}"
    v="${v:0:63}"
    while [[ "$v" =~ [^A-Za-z0-9]$ ]]; do v="${v%?}"; done
    echo "$v"
}

is_tree() { [[ "$1" =~ ^tree1:[0-9a-f]{64}$ ]]; }
tree_paths() { sql -c "SELECT path FROM pack_tree_files WHERE digest = '$1' ORDER BY path COLLATE \"C\""; }
tree_file() { sql -c "SELECT convert_from(content, 'UTF8') FROM pack_tree_files WHERE digest = '$1' AND path = '$2'"; }
tree_tarball() { sql -c "SELECT tarball_digest FROM pack_trees WHERE digest = '$1'"; }
draft_tree() { sql -c "SELECT tree_digest FROM playbook_draft_versions WHERE draft_id = '$1' AND version = $2"; }
playbook_tree() { sql -c "SELECT tree_digest FROM playbooks WHERE id = '$1'"; }
launch_tree() { sql -c "SELECT tree_digest FROM pack_tarballs WHERE issue_slug = '$(slug "$1")'"; }
listing() { tar -tzf "$1" | LC_ALL=C sort; }

# sql_file <file> [psql args...]: run the SQL on stdin with the file's bytes, as hex, in the psql
# variable hex. The bytes go over stdin, not the command line.
sql_file() {
    local hex
    hex=$(od -An -v -tx1 <"$1" | tr -d ' \n')
    shift
    { printf '%s\n' "\\set hex '$hex'"; cat; } | sql "$@"
}

# host_tarball <dir> <file>: the directory gzip-tarred by the host tar.
host_tarball() { (cd "$1" && COPYFILE_DISABLE=1 tar -czf - .) >"$2"; }

# sql_tarball <dir> [psql args...]: sql_file with the directory gzip-tarred by the host tar.
sql_tarball() {
    local dir="$1"
    shift
    host_tarball "$dir" "$WORK/sql-tarball.tar.gz"
    sql_file "$WORK/sql-tarball.tar.gz" "$@"
}

# seed_pack_row <slug> <file>: store the gzipped tarball as a launch pack row with no tree, as a
# controller from before tree storage wrote it.
seed_pack_row() {
    sql_file "$2" -v slug="$1" <<'SQL'
INSERT INTO pack_tarballs (issue_slug, tar_gz, digest, bytes, created_at)
SELECT :'slug', b, 'sha256:' || encode(sha256(b), 'hex'), length(b), now()::TEXT
FROM (SELECT decode(:'hex', 'hex') AS b) s;
SQL
}

# newest_launch <playbook> <origin>: the newest launch key of that origin, empty when none.
newest_launch() {
    sql -c "SELECT key FROM playbook_launches WHERE playbook = '$1' AND origin = '$2' ORDER BY key DESC LIMIT 1"
}

# new_launch <playbook> <origin> <previous key>: a launch newer than the previous one exists.
new_launch() { [ "$(newest_launch "$1" "$2")" != "$3" ]; }

launch_status() { crux playbook-run "$1" | jq -r .launch.status; }
is_running() { [ "$(launch_status "$1")" = running ]; }

git_commit() {
    git -C "$1" add -A
    git -C "$1" -c user.name=e2e -c user.email=e2e@example.com commit -qm "$2"
}

# ---- pack fixtures -------------------------------------------------------------------------
PACK="$WORK/deliver"
cp -R "$FIX/packs/deliver" "$PACK"
mkdir -p "$PACK/bulk"
for f in a b c; do head -c 512000 <(yes "pack delivery filler $f") >"$PACK/bulk/$f.txt"; done

# small_pack <dir> <check.sh body> [inject array]: a one-task pack whose task runs the given
# shell, injecting check.sh unless another inject array is given.
small_pack() {
    mkdir -p "$1"
    cp "$FIX/packs/deliver/workflow.star" "$1/"
    sed "s|^inject = .*|inject = ${3:-[\"check.sh\"]}|" "$FIX/packs/deliver/crucible.toml" >"$1/crucible.toml"
    printf '#!/bin/sh\nset -eu\n%s\n' "$2" >"$1/check.sh"
}

OK_JSON="echo '{\"ok\":true}'"

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

# pods_seen <watch label> <key>: how many pod events the watch saw for the launch.
pods_seen() {
    jq -s --arg key "$(label "$2")" '[.[] | select(.object.metadata.labels["crucible.dev/issue-key"] == $key)] | length' "$WORK/pods-$1.json"
}

# expect_no_pod <label> <key> [watch label]: the pod watch saw no pod for the settled launch, and
# it recorded no run.
expect_no_pod() {
    [ "$(pods_seen "${3:-$1}" "$2")" = 0 ] || fail "[$1] a pod was created for $2"
    [ "$(jq '.runs | length' "$WORK/run-$1.json")" = 0 ] || fail "[$1] $2 recorded a run"
    pass "[$1] no pod and no run for $2"
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

# delivered <label> <key> <none|inputs|legacy> [watch label] [tree]: assert, from the watch
# streams, how the launch's pack reached its pod. The pack key is the tarball of the given tree,
# else of the tree on the launch's pack row, or for legacy a launch whose row holds no tree; inputs
# adds the run's own inputs key, which pack-stage extracts over the pack. Writes
# delivered-<label>.tar.gz (and inputs-<label>.tar.gz), and the tree to LAUNCH_TREE.
delivered() {
    local label="$1" key="$2" mode="$3" watch="${4:-$1}" pod cm args want=pack.tar.gz
    [ "$mode" = inputs ] && want=inputs.tar.gz,pack.tar.gz
    pod=$(jq -rs --arg key "$(label "$key")" '[.[] | .object | select(.metadata.labels["crucible.dev/issue-key"] == $key and any(.spec.initContainers[]?; .name == "pack-stage"))] | last' "$WORK/pods-$watch.json")
    [ "$pod" != null ] || fail "[$label] no pod with a pack-stage init container was seen for $key"
    [ "$(jq -r '[.status.initContainerStatuses[]? | select(.name == "pack-stage") | .state.terminated.exitCode] | first' <<<"$pod")" = 0 ] ||
        fail "[$label] pack-stage did not exit 0: $(jq -c '.status.initContainerStatuses' <<<"$pod")"
    args=$(jq -r '.spec.initContainers[] | select(.name == "pack-stage") | .args | join(" ")' <<<"$pod" | tr '\n' ' ')
    grep -q 'tar -xzf /opt/crucible/pack-src/pack.tar.gz' <<<"$args" || fail "[$label] pack-stage does not extract pack.tar.gz"
    if [ "$mode" = inputs ]; then
        grep -q 'tar -xzf /opt/crucible/pack-src/pack.tar.gz.*tar -xzf /opt/crucible/pack-src/inputs.tar.gz' <<<"$args" ||
            fail "[$label] pack-stage does not extract inputs.tar.gz over the pack: $args"
    elif grep -q inputs.tar.gz <<<"$args"; then
        fail "[$label] pack-stage extracts inputs for a run with none: $args"
    fi
    pass "[$label] pack-stage extracted $want"

    cm=$(jq -rs --arg uid "$(jq -r '.metadata.uid' <<<"$pod")" '[.[] | .object | select(.binaryData["pack.tar.gz"] != null and .metadata.ownerReferences[0].uid == $uid)] | last' "$WORK/cms-$watch.json")
    [ "$cm" != null ] || fail "[$label] no pack ConfigMap owned by the pod was seen"
    [ "$(jq -r '(.binaryData | keys | join(",")) + " " + ((.data // {}) | length | tostring) + " " + (.immutable | tostring)' <<<"$cm")" = "$want 0 true" ] ||
        fail "[$label] pack ConfigMap is not the immutable binary keys $want: $(jq -c '{binaryData: (.binaryData | keys), data, immutable}' <<<"$cm")"
    jq -r '.binaryData["pack.tar.gz"]' <<<"$cm" | base64 -d >"$WORK/delivered-$label.tar.gz"
    [ "$mode" = inputs ] && jq -r '.binaryData["inputs.tar.gz"]' <<<"$cm" | base64 -d >"$WORK/inputs-$label.tar.gz"

    LAUNCH_TREE=${5:-$(launch_tree "$key")}
    if [ "$mode" = legacy ]; then
        [ -z "$LAUNCH_TREE" ] || fail "[$label] the launch's pack row holds tree $LAUNCH_TREE"
        return
    fi
    is_tree "$LAUNCH_TREE" || fail "[$label] the launch's pack row holds tree '$LAUNCH_TREE'"
    [ "sha256:$(sha256 "$WORK/delivered-$label.tar.gz")" = "$(tree_tarball "$LAUNCH_TREE")" ] ||
        fail "[$label] the pack key is not the tarball of the launch's tree $LAUNCH_TREE"
    pass "[$label] the pack key is the tarball of the launch's tree"
}

# staged <watch label> <key>: the watch saw the launch's pack-stage exit 0.
staged() {
    [ "$(jq -s --arg key "$(label "$2")" '[.[] | .object | select(.metadata.labels["crucible.dev/issue-key"] == $key) | .status.initContainerStatuses[]? | select(.name == "pack-stage" and .state.terminated.exitCode == 0)] | length' "$WORK/pods-$1.json")" -gt 0 ]
}

# check_delivery <label> <key>: the delivery pack reached the pod as one gzipped key.
check_delivery() {
    local label="$1" gz raw listing
    delivered "$label" "$2" none
    gz=$(wc -c <"$WORK/delivered-$label.tar.gz" | tr -d ' ')
    raw=$(gzip -dc "$WORK/delivered-$label.tar.gz" | wc -c | tr -d ' ')
    [ "$gz" -le 921600 ] || fail "[$label] delivered tarball is $gz bytes, over the budget"
    [ "$raw" -gt 1048576 ] || fail "[$label] raw tar is only $raw bytes; the fixture no longer exceeds the ConfigMap limit"
    listing=$(listing "$WORK/delivered-$label.tar.gz" | tr '\n' ' ')
    [ "$listing" = "bulk/a.txt bulk/b.txt bulk/c.txt check.sh crucible.toml nested/deep/marker.txt workflow.star " ] ||
        fail "[$label] delivered tarball lists: $listing"
    pass "[$label] pack delivered as one gzipped key ($gz bytes gzipped, $raw raw)"
}

# launch_and_check <label> <crux launch command...>: launch the delivery pack, expect success,
# check delivery.
launch_and_check() {
    local label="$1"
    shift
    watch_start "$label"
    launch "$label" "$@"
    settle "$label" "$KEY"
    expect_finished "$label"
    check_delivery "$label" "$KEY"
    watch_stop
}

# launch_delivered <label> <none|inputs|legacy> <crux launch command...>: launch, expect success,
# check how the pack was delivered.
launch_delivered() {
    local label="$1" mode="$2"
    shift 2
    watch_start "$label"
    launch "$label" "$@"
    settle "$label" "$KEY"
    expect_finished "$label"
    delivered "$label" "$KEY" "$mode"
    watch_stop
}

# download <label> <draft> <version>: fetch the version's tarball into download-<label>.tar.gz;
# its X-Pack-Digest lands in DOWNLOADED.
download() {
    curl -sf -D "$WORK/download-$1.headers" -o "$WORK/download-$1.tar.gz" \
        "$CONTROLLER_URL/api/playbook-drafts/$2/tarball?version=$3" || fail "[$1] downloading $2 version $3 failed"
    DOWNLOADED=$(tr -d '\r' <"$WORK/download-$1.headers" | awk -F': ' 'tolower($1) == "x-pack-digest" { print $2 }')
    is_tree "$DOWNLOADED" || fail "[$1] the download's X-Pack-Digest is '$DOWNLOADED'"
}

# check_download <label> <draft> <version> <tree>: the version downloads as that tree's tarball,
# named in X-Pack-Digest.
check_download() {
    download "$1" "$2" "$3"
    [ "$DOWNLOADED" = "$4" ] || fail "[$1] X-Pack-Digest is $DOWNLOADED, not $4"
    [ "sha256:$(sha256 "$WORK/download-$1.tar.gz")" = "$(tree_tarball "$4")" ] ||
        fail "[$1] the download is not the tarball of $4"
    [ "$(listing "$WORK/download-$1.tar.gz")" = "$(tree_paths "$4")" ] ||
        fail "[$1] the download lists $(listing "$WORK/download-$1.tar.gz" | tr '\n' ' ')"
    pass "[$1] $2 version $3 downloads as its tree, named in X-Pack-Digest"
}

# draft_files <draft>: the paths `crux draft-files` serves for the newest save.
draft_files() { crux draft-files "$1" --json | jq -r '.files | keys[]' | LC_ALL=C sort; }

# expect_playbook_files <label> <playbook> <dir>: the registry inspector serves the files of the
# pack in dir, check.sh byte for byte.
expect_playbook_files() {
    api GET "/api/playbooks/$2"
    [ "$HTTP" = 200 ] || fail "[$1] inspecting $2: $HTTP $(cat "$WORK/api.json")"
    [ "$(jq -r '.files | keys[]' "$WORK/api.json" | LC_ALL=C sort)" = "$(cd "$3" && find . -type f | sed 's|^\./||' | LC_ALL=C sort)" ] ||
        fail "[$1] $2 serves files $(jq -c '.files | keys' "$WORK/api.json"), not those of $3"
    [ "$(jq -r '.files["check.sh"]' "$WORK/api.json")" = "$(cat "$3/check.sh")" ] ||
        fail "[$1] $2 serves a check.sh that is not the one in $3"
    pass "[$1] the registry inspector serves $2 as the pack in $(basename "$3")"
}

# clone_template <label> <draft> <playbook> <tree>: a draft templated from the playbook at the
# revision and tree digest it serves stores that tree as its version 1. A row with no tree yet is
# templated by its revision alone.
clone_template() {
    local rev digest
    read -r rev digest <<<"$(sql -F ' ' -c "SELECT rev, tree_digest FROM playbooks WHERE id = '$3'")"
    api POST /api/playbook-drafts "$(jq -nc --arg id "$2" --arg t "$3" --arg rev "$rev" --arg d "$digest" \
        '{id: $id, description: "kind e2e", template: $t, template_rev: $rev}
         + (if $d == "" then {} else {template_digest: $d} end)')"
    [ "$HTTP" = 201 ] || fail "[$1] templating $2 from $3: $HTTP $(cat "$WORK/api.json")"
    [ "$(draft_tree "$2" 1)" = "$4" ] || fail "[$1] the draft templated from $3 holds '$(draft_tree "$2" 1)', not $4"
    pass "[$1] a draft templated from $3 holds $4"
}

# insert_steering <key> <seq> <body>: record a steering row for a launch, stamped 2026-10-01Z.
insert_steering() {
    { printf '%s\n' "\\set body '$3'"; cat; } <<'SQL' | sql -v slug="$(slug "$1")" -v seq="$2"
INSERT INTO pack_steering (issue_slug, seq, body_md, author, created_at)
VALUES (:'slug', :'seq', :'body', 'e2e', '2026-10-01T00:00:00Z');
SQL
}
STEER_STAMP='<!-- steer @1790812800 by control -->'

# ---- scenario A ----------------------------------------------------------------------------
start_controller
[ "$(crux whoami 2>/dev/null | head -n1)" != "" ] || fail "crux cannot reach the controller"

log "[A] creating draft $DRAFT and saving the delivery pack"
new_draft "$DRAFT" "$PACK"
VERSION=2
TREE=$(draft_tree "$DRAFT" "$VERSION")
is_tree "$TREE" || fail "[A] the save recorded tree digest '$TREE'"
tree_paths "$TREE" | grep -qx 'nested/deep/marker.txt' || fail "[A] the stored tree is missing nested/deep/marker.txt"
check_download A "$DRAFT" "$VERSION" "$TREE"
launch_and_check A draft-launch "$DRAFT"
[ "$LAUNCH_TREE" = "$TREE" ] || fail "[A] the launch ran tree $LAUNCH_TREE, not the saved $TREE"
wait_for 90 "the run's pod and pack ConfigMap to be collected" no_run_objects
pass "[A] pod and pack ConfigMap collected"
[ "$(jq -c '[.local_tree_digest, .ignored]' "$WORK/push-$DRAFT.json")" = "[\"$TREE\",[]]" ] ||
    fail "[A] the push reported $(jq -c '[.local_tree_digest, .ignored]' "$WORK/push-$DRAFT.json"), not the saved $TREE"
pass "[A] the push reported the saved tree as the directory's local digest"
api GET "/api/playbook-drafts/$DRAFT"
expect_tree_digest A ".versions[] | select(.version == $VERSION)" "$TREE"
curl -sf "$CONTROLLER_URL/api/openapi.json" >"$WORK/openapi.json" || fail "[A] the OpenAPI spec is not served"
if ! { grep -q '"tree_digest"' "$WORK/openapi.json" && grep -q '"adopted_tree_digest"' "$WORK/openapi.json"; }; then
    fail "[A] the OpenAPI spec names no tree_digest"
fi
grep -q 'tar_digest' "$WORK/openapi.json" && fail "[A] the OpenAPI spec still names tar_digest"
pass "[A] the draft API and the OpenAPI spec name the pack digest tree_digest, never tar_digest"

log "[A] pushing and pulling a pack dir through crux's walker"
WALK="$WORK/walk"
small_pack "$WALK" "$OK_JSON"
mkdir -p "$WALK/state" "$WALK/workspace" "$WALK/.git" "$WALK/sub/state"
for f in state/cursor workspace/main.go .git/HEAD sub/state/x; do printf '\377\376' >"$WALK/$f"; done
echo kept >"$WALK/sub/kept.txt"
crux draft-create "$DRAFT-walk" --description "kind e2e" >/dev/null
if crux draft-push "$DRAFT-walk" "$WALK" --base 1 >/dev/null 2>&1; then
    fail "[A] draft-push still takes --base"
fi
ln -s check.sh "$WALK/link"
if crux draft-push "$DRAFT-walk" "$WALK" --base-version 1 >"$WORK/push-walk-link.log" 2>&1; then
    fail "[A] a pack dir holding a symlink was pushed"
fi
grep -q 'link is a symbolic link' "$WORK/push-walk-link.log" ||
    fail "[A] the symlink refusal does not name it: $(cat "$WORK/push-walk-link.log")"
rm "$WALK/link"
ln -s /etc/passwd "$WALK/workspace/link"
crux draft-push "$DRAFT-walk" "$WALK" --base-version 1 >"$WORK/push-walk.txt"
WALK_TREE=$(draft_tree "$DRAFT-walk" 2)
is_tree "$WALK_TREE" || fail "[A] the walked push saved tree '$WALK_TREE': $(cat "$WORK/push-walk.txt")"
if ! { grep -qx "local tree: $WALK_TREE" "$WORK/push-walk.txt" &&
    grep -qx 'ignored: .git, state, sub/state, workspace' "$WORK/push-walk.txt"; }; then
    fail "[A] the push printed: $(cat "$WORK/push-walk.txt")"
fi
[ "$(tree_paths "$WALK_TREE" | tr '\n' ' ')" = "check.sh crucible.toml sub/kept.txt workflow.star " ] ||
    fail "[A] the walked push saved $(tree_paths "$WALK_TREE" | tr '\n' ' ')"
pass "[A] draft-push skipped state/, workspace/ and .git at any depth, refused a symlink, and printed the local tree the save stored"
mkdir -p "$WORK/pulled/state"
echo local >"$WORK/pulled/state/cursor"
crux draft-pull "$DRAFT-walk" "$WORK/pulled" >"$WORK/pull-walk.txt"
if ! { grep -qx "local tree: $WALK_TREE" "$WORK/pull-walk.txt" && grep -qx 'ignored: state' "$WORK/pull-walk.txt"; }; then
    fail "[A] the pull printed: $(cat "$WORK/pull-walk.txt")"
fi
crux draft-pull "$DRAFT-walk" "$WORK/pulled" --json >"$WORK/pull-walk.json"
[ "$(jq -c '[.local_tree_digest, .ignored]' "$WORK/pull-walk.json")" = "[\"$WALK_TREE\",[\"state\"]]" ] ||
    fail "[A] the pull reported $(jq -c '[.local_tree_digest, .ignored]' "$WORK/pull-walk.json")"
pass "[A] draft-pull printed the pulled dir's local tree and the paths a push would skip"

# ---- scenario P ----------------------------------------------------------------------------
log "[P] publishing $DRAFT as playbook $DRAFT-pub"
crux draft-publish "$DRAFT" --playbook "$DRAFT-pub" --json >"$WORK/publish-P.json"
[ "$(jq -r .id "$WORK/publish-P.json")" = "$DRAFT-pub" ] || fail "[P] publish: $(cat "$WORK/publish-P.json")"
[ "$(playbook_tree "$DRAFT-pub")" = "$TREE" ] || fail "[P] the registry row holds tree '$(playbook_tree "$DRAFT-pub")'"
[ "$(sql -c "SELECT count(*) FROM playbook_revisions WHERE playbook_id = '$DRAFT-pub' AND tree_digest = '$TREE'")" = 1 ] ||
    fail "[P] the published tree is not recorded as a revision"
[ "$(sql -c "SELECT rev FROM playbooks WHERE id = '$DRAFT-pub'")" = "$TREE" ] ||
    fail "[P] the published row's rev is $(sql -c "SELECT rev FROM playbooks WHERE id = '$DRAFT-pub'"), not its tree"
pass "[P] the registry row and its revision hold the draft's tree, and its rev is that tree"
launch_and_check P launch "$DRAFT-pub"
[ "$LAUNCH_TREE" = "$TREE" ] || fail "[P] the launch ran tree $LAUNCH_TREE, not the registered $TREE"
read -r launched registered <<<"$(sql -F ' ' -c "SELECT l.exposure_digest, p.exposure_digest FROM playbook_launches l JOIN playbooks p ON p.id = l.playbook WHERE l.key = '$KEY'")"
[ -n "$registered" ] && [ "$launched" = "$registered" ] ||
    fail "[P] the launch recorded exposure '$launched'; the registry row holds '$registered'"
pass "[P] the launch recorded the registry row's exposure"
expect_playbook_files P "$DRAFT-pub" "$PACK"
expect_tree_digest P . "$TREE"
api GET /api/playbooks
expect_tree_digest P ".[] | select(.id == \"$DRAFT-pub\")" "$TREE"
[ "$(jq '[.[] | has("tar_digest")] | any' "$WORK/api.json")" = false ] || fail "[P] the registry list still carries tar_digest"
pass "[P] the registry inspector and list name the registered tree_digest"
clone_template P e2e-clone "$DRAFT-pub" "$TREE"

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
$OK_JSON"
small_pack "$WORK/long" "sleep 45
$OK_JSON"
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
log "[B] seeding a symlink pack and an archive past the expansion bound"
mkdir -p "$WORK/symlink"
echo m >"$WORK/symlink/crucible.toml"
ln -s crucible.toml "$WORK/symlink/link"
mkdir -p "$WORK/bomb"
head -c 70000000 /dev/zero >"$WORK/bomb/zeros"
for seed in symlink bomb; do
    host_tarball "$WORK/$seed" "$WORK/$seed.tar.gz"
    seed_pack_row "kind_e2e_$seed" "$WORK/$seed.tar.gz"
done
rm "$WORK/bomb.tar.gz"
rm -rf "$WORK/bomb"

start_controller
read -r converted unconvertible <<<"$(conversion_report "$BOOT")"
[ "$unconvertible" = 2 ] || fail "[B] boot $BOOT reported '$converted $unconvertible'; expected two unconvertible packs"
[ "$(sql -c "SELECT count(*) FROM playbook_draft_versions WHERE tree_digest IS NULL")" = 0 ] ||
    fail "[B] draft versions are left unconverted"
[ "$(sql -c "SELECT count(*) FROM pack_digest_aliases WHERE unconvertible_reason IS NOT NULL")" = 2 ] ||
    fail "[B] the symlink pack and the bomb are not both recorded unconvertible"
sql -c "SELECT a.unconvertible_reason FROM pack_digest_aliases a JOIN pack_tarballs t ON t.digest = a.old_digest WHERE t.issue_slug = 'kind_e2e_bomb'" |
    grep -q 'expands past 67108864 bytes' || fail "[B] the bomb is not recorded as expanding past the bound"
[ "$(draft_tree "$DRAFT" "$VERSION")" = "$TREE" ] || fail "[B] version $VERSION no longer holds its saved tree"
pass "[B] every pack row holds a tree, and the symlink pack and the bomb are recorded unconvertible"

stop_controller
start_controller
[ "$(conversion_report "$BOOT")" = none ] || fail "[B] boot $BOOT converted again: $(conversion_report "$BOOT")"
pass "[B] a restart with nothing to convert stays silent"

stop_controller
log "[B] rewriting version $VERSION with a host tar encoding of the same tree"
sql_tarball "$PACK" -v draft="$DRAFT" -v version="$VERSION" <<'SQL'
UPDATE playbook_draft_versions
SET tar_gz = s.b, tar_digest = 'sha256:' || encode(sha256(s.b), 'hex'), tar_bytes = length(s.b)
FROM (SELECT decode(:'hex', 'hex') AS b) s
WHERE draft_id = :'draft' AND version = :'version';
SQL
[ -z "$(draft_tree "$DRAFT" "$VERSION")" ] || fail "[B] rewriting the legacy bytes alone did not clear the tree digest"

start_controller
read -r converted unconvertible <<<"$(conversion_report "$BOOT")"
[ "$converted $unconvertible" = "1 0" ] ||
    fail "[B] boot $BOOT reported '$converted $unconvertible'; expected only the rewritten version"
RETREE=$(draft_tree "$DRAFT" "$VERSION")
[ "$RETREE" = "$TREE" ] || fail "[B] the host tar encoding converted to a different tree:
$(diff <(sql -c "SELECT path, sha256 FROM pack_tree_files WHERE digest = '$TREE' ORDER BY path") \
    <(sql -c "SELECT path, sha256 FROM pack_tree_files WHERE digest = '$RETREE' ORDER BY path"))"
pass "[B] a foreign encoding of the same files converted to the same tree"
launch_and_check B draft-launch "$DRAFT"

log "[B] pointing version $VERSION's legacy bytes at a different pack, keeping its tree"
small_pack "$WORK/decoy" 'echo decoy'
sql_tarball "$WORK/decoy" -v draft="$DRAFT" -v version="$VERSION" <<'SQL'
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
[ "$(draft_tree "$DRAFT" "$VERSION")" = "$TREE" ] || fail "[B] version $VERSION lost its tree when its bytes were rewritten"
launch_and_check B-tree draft-launch "$DRAFT"
[ "$(sql -c "SELECT count(*) FROM pack_tarballs WHERE tree_digest = '$TREE' AND digest = '$DECOY_DIGEST'")" = 1 ] ||
    fail "[B] the launch did not carry the decoy bytes beside the tree"
pass "[B] dispatch delivered the stored tree, not the launch's legacy bytes"

# ---- scenario D ----------------------------------------------------------------------------
check_download D "$DRAFT" "$VERSION" "$TREE"
[ "$(draft_files "$DRAFT")" = "$(tree_paths "$TREE")" ] ||
    fail "[D] draft-files serves $(draft_files "$DRAFT" | tr '\n' ' ')"
pass "[D] draft-files serves the stored tree, not the decoy bytes"

log "[D] saving a draft while its launch waits on the version row"
small_pack "$WORK/race" "$OK_JSON"
new_draft "$DRAFT-race" "$WORK/race"
hold_lock "SELECT 1 FROM playbook_draft_versions WHERE draft_id = '$DRAFT-race' AND version = 2 FOR UPDATE"
api_bg race POST "/api/playbook-drafts/$DRAFT-race/launch" '{"max_cost":1,"max_time":"5m"}'
wait_for 60 "the draft launch to wait on the version row" lock_waiters 1
echo '# a later save' >>"$WORK/race/check.sh"
crux draft-push "$DRAFT-race" "$WORK/race" --base-version 2 --json >"$WORK/push-race.json"
[ "$(jq -r .version "$WORK/push-race.json")" = 3 ] || fail "[D] the racing save: $(cat "$WORK/push-race.json")"
release_lock
api_wait
expect_bg D race 409 "was saved while the launch was being authorized (now version 3)"

log "[D] deleting a draft while its launch waits on the version row"
hold_lock "DELETE FROM playbook_drafts WHERE id = '$DRAFT-race'"
api_bg gone POST "/api/playbook-drafts/$DRAFT-race/launch" '{"max_cost":1,"max_time":"5m"}'
wait_for 60 "the draft launch to wait on the deleted version row" lock_waiters 1
release_lock
api_wait
expect_bg D gone 404 "no draft \\\"$DRAFT-race\\\""
[ "$(sql -c "SELECT count(*) FROM playbook_launches WHERE playbook = '$DRAFT-race'")" = 0 ] ||
    fail "[D] a refused draft launch left a launch row"
pass "[D] a save or a delete under a waiting draft launch refuses it with 409 or 404"

# ---- scenario L ----------------------------------------------------------------------------
log "[L] rewriting version $VERSION's bytes to another pack on the live controller"
small_pack "$WORK/legacy" "test \"\$(cat marker.txt)\" = legacy-bytes
$OK_JSON" '["check.sh", "marker.txt"]'
echo legacy-bytes >"$WORK/legacy/marker.txt"
sql_tarball "$WORK/legacy" -v draft="$DRAFT" -v version="$VERSION" <<'SQL'
UPDATE playbook_draft_versions
SET tar_gz = s.b, tar_digest = 'sha256:' || encode(sha256(s.b), 'hex'), tar_bytes = length(s.b)
FROM (SELECT decode(:'hex', 'hex') AS b) s
WHERE draft_id = :'draft' AND version = :'version';
SQL
[ -z "$(draft_tree "$DRAFT" "$VERSION")" ] || fail "[L] new bytes that are no encoding of the tree kept it"
LEGACY_FILES="check.sh crucible.toml marker.txt workflow.star "
download L "$DRAFT" "$VERSION"
LEGACY_TREE="$DOWNLOADED"
[ "$LEGACY_TREE" != "$TREE" ] || fail "[L] the legacy bytes downloaded as the old tree"
[ "$(listing "$WORK/download-L.tar.gz" | tr '\n' ' ')" = "$LEGACY_FILES" ] ||
    fail "[L] the download lists $(listing "$WORK/download-L.tar.gz" | tr '\n' ' ')"
[ "$(draft_files "$DRAFT" | tr '\n' ' ')" = "$LEGACY_FILES" ] ||
    fail "[L] draft-files serves $(draft_files "$DRAFT" | tr '\n' ' ')"
pass "[L] the download and draft-files read the legacy bytes"
launch_delivered L legacy draft-launch "$DRAFT"
[ "$(listing "$WORK/delivered-L.tar.gz" | tr '\n' ' ')" = "$LEGACY_FILES" ] ||
    fail "[L] the run received $(listing "$WORK/delivered-L.tar.gz" | tr '\n' ' ')"
[ "sha256:$(sha256 "$WORK/delivered-L.tar.gz")" = "sha256:$(sha256 "$WORK/download-L.tar.gz")" ] ||
    fail "[L] the run did not receive the pack the download served"
LEGACY_KEY="$KEY"
pass "[L] the run received the legacy bytes' tree"

log "[L] rewriting a registered playbook's bytes to the same pack on the live controller"
small_pack "$WORK/lreg" "$OK_JSON"
new_draft "$DRAFT-lreg" "$WORK/lreg"
crux draft-publish "$DRAFT-lreg" --playbook "$DRAFT-lregpub" --json >"$WORK/publish-L.json"
sql_tarball "$WORK/legacy" -v id="$DRAFT-lregpub" <<'SQL'
UPDATE playbooks
SET tar_gz = s.b, tar_digest = 'sha256:' || encode(sha256(s.b), 'hex'), tar_bytes = length(s.b)
FROM (SELECT decode(:'hex', 'hex') AS b) s
WHERE id = :'id';
SQL
[ -z "$(playbook_tree "$DRAFT-lregpub")" ] || fail "[L] new registry bytes that are no encoding of the tree kept it"
expect_playbook_files L-reg "$DRAFT-lregpub" "$WORK/legacy"
clone_template L-reg e2e-lclone "$DRAFT-lregpub" "$LEGACY_TREE"
launch_delivered L-reg legacy launch "$DRAFT-lregpub"
[ "$(listing "$WORK/delivered-L-reg.tar.gz" | tr '\n' ' ')" = "$LEGACY_FILES" ] ||
    fail "[L] the registered launch received $(listing "$WORK/delivered-L-reg.tar.gz" | tr '\n' ' ')"
LEGACY_REG_KEY="$KEY"
read -r launched registered <<<"$(sql -F ' ' -c "SELECT l.exposure_digest, p.exposure_digest FROM playbook_launches l JOIN playbooks p ON p.id = l.playbook WHERE l.key = '$KEY'")"
[ -n "$registered" ] && [ "$launched" = "$registered" ] ||
    fail "[L] the legacy launch recorded exposure '$launched'; the registry row holds '$registered'"
pass "[L] a registry row's legacy bytes are inspected, templated, and launched, pinned by their digest"

# ---- scenario T ----------------------------------------------------------------------------
log "[T] tampering with a published tree"
small_pack "$WORK/tamper" "echo tamper-$ID
$OK_JSON"
new_draft "$DRAFT-tamper" "$WORK/tamper"
crux draft-publish "$DRAFT-tamper" --playbook "$DRAFT-tpub" --json >"$WORK/publish-T.json"
TAMPER_TREE=$(playbook_tree "$DRAFT-tpub")
[ "$TAMPER_TREE" = "$(draft_tree "$DRAFT-tamper" 2)" ] || fail "[T] the published row does not hold the draft's tree"
sql -c "UPDATE pack_tree_files SET content = 'tampered' WHERE digest = '$TAMPER_TREE' AND path = 'check.sh'"
if crux draft-launch "$DRAFT-tamper" --max-cost 1 --max-time 5m >"$WORK/launch-T-draft.log" 2>&1; then
    fail "[T] a draft whose stored tree was tampered with launched"
fi
grep -qF "$TAMPER_TREE no longer matches its stored files" "$WORK/launch-T-draft.log" ||
    fail "[T] the draft launch refusal does not name the tree: $(cat "$WORK/launch-T-draft.log")"
pass "[T] the draft launch is refused naming the tampered tree"
watch_start T
launch T launch "$DRAFT-tpub"
settle T "$KEY"
watch_stop
expect_parked T "$TAMPER_TREE no longer matches its stored files"
expect_no_pod T "$KEY"

# ---- scenario I ----------------------------------------------------------------------------
log "[I] importing a pack from a local git repo"
REPO="$WORK/repo"
small_pack "$REPO/pack" "echo git-import-$ID
$OK_JSON"
mkdir -p "$REPO/pack/state"
echo junk >"$REPO/pack/state/junk.txt"
git -C "$REPO" init -q
git_commit "$REPO" "pack"
GIT_SOURCE="\"repo\":\"file://$REPO\",\"path\":\"pack\""
IMPORT_FILES="check.sh crucible.toml workflow.star "

api POST /api/playbooks/imports "{$GIT_SOURCE}"
expect_http I 201 '"ignored_paths":["state"]'
IMPORT=$(jq -r .id "$WORK/api.json")
IMPORT_TREE=$(sql -c "SELECT tree_digest FROM pack_imports WHERE id = '$IMPORT'")
is_tree "$IMPORT_TREE" || fail "[I] the import recorded tree '$IMPORT_TREE'"
expect_tree_digest I . "$IMPORT_TREE"
crux playbook-import "file://$REPO" --path pack >"$WORK/import-I.txt"
grep -qx "tree: $IMPORT_TREE" "$WORK/import-I.txt" || fail "[I] crux printed the import as: $(cat "$WORK/import-I.txt")"
crux playbook-import "file://$REPO" --path pack --json >"$WORK/api.json"
expect_tree_digest I . "$IMPORT_TREE"
pass "[I] the import API and crux name the frozen tree_digest"
[ "$(tree_paths "$IMPORT_TREE" | tr '\n' ' ')" = "$IMPORT_FILES" ] ||
    fail "[I] the import stored $(tree_paths "$IMPORT_TREE" | tr '\n' ' ')"
api GET "/api/playbooks/imports/$IMPORT"
[ "$HTTP" = 200 ] && [ "$(jq 'has("ignored_paths")' "$WORK/api.json")" = false ] ||
    fail "[I] the import read back reports ignored paths: $(cat "$WORK/api.json")"
pass "[I] the proposal stored the tree without state/ and reported it once"

api POST "/api/playbooks/imports/$IMPORT/draft" '{"id":"e2e-imported","description":"kind e2e"}'
expect_http I 201 '"version":1'
[ "$(draft_tree e2e-imported 1)" = "$IMPORT_TREE" ] || fail "[I] the draft opened from the import holds '$(draft_tree e2e-imported 1)'"
api GET /api/playbook-drafts/e2e-imported/origin/files
[ "$HTTP" = 200 ] && [ "$(jq -r '.files | keys | join(" ")' "$WORK/api.json") " = "$IMPORT_FILES" ] ||
    fail "[I] the draft's origin files: $HTTP $(cat "$WORK/api.json")"
pass "[I] the import opened as a draft holding its tree"

api POST /api/playbooks/imports "{$GIT_SOURCE}"
expect_http I 201 '"ignored_paths":["state"]'
IMPORT=$(jq -r .id "$WORK/api.json")
api POST "/api/playbooks/imports/$IMPORT/compile" '{}'
expect_http I 200 '"schema_digest":"sha256:'
api POST "/api/playbooks/imports/$IMPORT/register" '{"id":"e2e-imp-pub","description":"kind e2e"}'
expect_http I 201 '"ignored_paths":["state"]'
expect_tree_digest I . "$IMPORT_TREE"
[ "$(playbook_tree e2e-imp-pub)" = "$IMPORT_TREE" ] || fail "[I] the registered import holds '$(playbook_tree e2e-imp-pub)'"
pass "[I] the compiled import registered its tree"

api POST /api/playbook-drafts/from-git "{\"id\":\"e2e-fromgit\",\"description\":\"kind e2e\",$GIT_SOURCE}"
expect_http I 201 '"ignored_paths":["state"]'
[ "$(draft_tree e2e-fromgit 1)" = "$IMPORT_TREE" ] || fail "[I] the draft from git holds '$(draft_tree e2e-fromgit 1)'"
api POST /api/playbooks "{\"id\":\"e2e-git\",\"description\":\"kind e2e\",$GIT_SOURCE}"
expect_http I 201 '"ignored_paths":["state"]'
[ "$(playbook_tree e2e-git)" = "$IMPORT_TREE" ] || fail "[I] the git registration holds '$(playbook_tree e2e-git)'"
pass "[I] a draft from git and a git registration hold the same tree"
launch_delivered I none launch e2e-git
[ "$LAUNCH_TREE" = "$IMPORT_TREE" ] || fail "[I] the launch ran $LAUNCH_TREE"
crux playbooks >"$WORK/playbooks-I.txt"
GIT_REV=$(sql -c "SELECT rev FROM playbooks WHERE id = 'e2e-git'")
if ! { grep -E "^$DRAFT-pub " "$WORK/playbooks-I.txt" | grep -qF " ${TREE:0:18} " &&
    grep -E "^e2e-git " "$WORK/playbooks-I.txt" | grep -qF " ${GIT_REV:0:12} "; }; then
    fail "[I] crux playbooks does not shorten a tree rev to its prefix and 12 characters, or a commit to 12: $(cat "$WORK/playbooks-I.txt")"
fi
pass "[I] crux playbooks shows a draft-sourced rev as tree1: and 12 characters"

HEAVY="$WORK/heavy"
cp -R "$OVER" "$HEAVY"
git -C "$HEAVY" init -q
git_commit "$HEAVY" "heavy"
api POST /api/playbooks "{\"id\":\"e2e-heavy\",\"description\":\"kind e2e\",\"repo\":\"file://$HEAVY\"}"
expect_http I 422 'delivery budget'
api POST /api/playbooks/imports "{\"repo\":\"file://$HEAVY\"}"
expect_http I 422 'delivery budget'
[ -z "$(playbook_tree e2e-heavy)" ] || fail "[I] the over-budget repo registered"
pass "[I] an over-budget repo is refused at registration and at proposal"

# ---- scenario W (registration) -------------------------------------------------------------
log "[W] registering the webhook playbook at v1"
WREPO="$WORK/wrepo"
small_pack "$WREPO/pack" "test \"\$(cat marker.txt)\" = v1
$OK_JSON" '["check.sh", "marker.txt"]'
echo v1 >"$WREPO/pack/marker.txt"
printf '\n[outputs.gpu-capture]\ncount = 1\n' >>"$WREPO/pack/crucible.toml"
git -C "$WREPO" init -q
git_commit "$WREPO" v1
W_SOURCE="\"repo\":\"file://$WREPO\",\"path\":\"pack\""
api POST /api/playbooks "{\"id\":\"e2e-hook\",\"description\":\"kind e2e\",$W_SOURCE}"
expect_http W 201 '"id":"e2e-hook"'
W_TREE=$(playbook_tree e2e-hook)
W_EXPOSURE=$(sql -c "SELECT exposure_digest FROM playbooks WHERE id = 'e2e-hook'")
is_tree "$W_TREE" && [ -n "$W_EXPOSURE" ] || fail "[W] v1 registered with tree '$W_TREE', exposure '$W_EXPOSURE'"

# ---- scenario E ----------------------------------------------------------------------------
stop_controller
log "[E] rewriting e2e-hook with a host tar encoding, staling $DRAFT-pub, and clearing two agents"
sql_tarball "$WREPO/pack" <<'SQL'
UPDATE playbooks
SET tar_gz = s.b, tar_digest = 'sha256:' || encode(sha256(s.b), 'hex'), tar_bytes = length(s.b)
FROM (SELECT decode(:'hex', 'hex') AS b) s
WHERE id = 'e2e-hook';
SQL
[ -z "$(playbook_tree e2e-hook)" ] || fail "[W] rewriting e2e-hook's bytes kept its tree"
W_FOREIGN=$(sql -c "SELECT tar_digest FROM playbooks WHERE id = 'e2e-hook'")

mkdir -p "$WORK/param"
cp "$FIX/packs/deliver/crucible.toml" "$WORK/param/"
printf 'params = {"topic": {"type": "string", "required": True}}\n\n%s\n' "$(cat "$FIX/packs/deliver/workflow.star")" >"$WORK/param/workflow.star"
PUB_SCHEMA=$(sql -c "SELECT schema_digest FROM playbooks WHERE id = '$DRAFT-pub'")
sql_tarball "$WORK/param" -v id="$DRAFT-pub" <<'SQL'
BEGIN;
ALTER TABLE playbooks DISABLE TRIGGER playbooks_legacy_bytes;
UPDATE playbooks
SET tar_gz = s.b, tar_digest = 'sha256:' || encode(sha256(s.b), 'hex'), tar_bytes = length(s.b),
    core_rev = 'stale'
FROM (SELECT decode(:'hex', 'hex') AS b) s
WHERE id = :'id';
ALTER TABLE playbooks ENABLE TRIGGER playbooks_legacy_bytes;
COMMIT;
SQL
sql -c "UPDATE playbooks SET agent_backend = NULL WHERE id IN ('$DRAFT-pub', '$DRAFT-tpub')"

CONTROLLER_ENV=(CONTROLLER_MAX_CONCURRENT_PODS=1)
start_controller
[ "$(playbook_tree e2e-hook)" = "$W_TREE" ] || fail "[W] the host tar encoding of v1 converted to '$(playbook_tree e2e-hook)'"
[ "$(sql -c "SELECT tree_digest FROM pack_digest_aliases WHERE old_digest = '$W_FOREIGN'")" = "$W_TREE" ] ||
    fail "[W] the conversion recorded no alias for the foreign encoding"
pass "[W] the foreign encoding converted to v1's tree and is recorded as its alias"
[ "$(draft_tree "$DRAFT" "$VERSION")" = "$LEGACY_TREE" ] ||
    fail "[L] version $VERSION converted to '$(draft_tree "$DRAFT" "$VERSION")', not the downloaded $LEGACY_TREE"
[ "$(launch_tree "$LEGACY_KEY")" = "$LEGACY_TREE" ] || fail "[L] the legacy launch's pack row converted to '$(launch_tree "$LEGACY_KEY")'"
[ "$(playbook_tree "$DRAFT-lregpub")" = "$LEGACY_TREE" ] && [ "$(launch_tree "$LEGACY_REG_KEY")" = "$LEGACY_TREE" ] ||
    fail "[L] the legacy registry row and its launch converted to '$(playbook_tree "$DRAFT-lregpub")' and '$(launch_tree "$LEGACY_REG_KEY")'"
pass "[L] the legacy bytes converted to the tree their download named"
read -r core schema <<<"$(sql -F ' ' -c "SELECT core_rev, schema_digest FROM playbooks WHERE id = '$DRAFT-pub'")"
[ "$core" != stale ] && [ "$schema" = "$PUB_SCHEMA" ] ||
    fail "[E] $DRAFT-pub re-derived to core_rev '$core', schema $schema (was $PUB_SCHEMA)"
pass "[E] the stale row re-derived its form from its tree, not its decoy bytes"
[ "$(sql -c "SELECT agent_backend FROM playbooks WHERE id = '$DRAFT-pub'")" = command ] ||
    fail "[T] the backfill did not stamp $DRAFT-pub"
[ -z "$(sql -c "SELECT agent_backend FROM playbooks WHERE id = '$DRAFT-tpub'")" ] || fail "[T] the backfill stamped the tampered pack"
grep 'pack agent backfill skipped a pack it cannot read' "$WORK/controller-$BOOT.log" | grep -q "key=$DRAFT-tpub" ||
    fail "[T] the backfill did not report skipping $DRAFT-tpub"
pass "[T] the agent backfill skipped the tampered tree and stamped the rest"

# ---- scenario W ----------------------------------------------------------------------------
log "[W] creating a webhook on the converted v1, then re-registering v2"
jq -n '{playbook: "e2e-hook", verifier: "path_token", dedupe: "string(body.n)", max_launches_per_hour: 10, max_cost: 1, max_time: "5m"}' >"$WORK/webhook-W.json"
crux webhook-create --file "$WORK/webhook-W.json" >"$WORK/webhook-created-W.json"
WEBHOOK=$(jq -r .webhook.id "$WORK/webhook-created-W.json")
WEBHOOK_TOKEN=$(jq -r .secret "$WORK/webhook-created-W.json")
[ "$(sql -F ' ' -c "SELECT adopted_tree_digest, adopted_tar_digest FROM playbook_standing_launches WHERE id = '$WEBHOOK'")" = "$W_TREE $W_FOREIGN" ] ||
    fail "[W] the webhook adopted $(sql -F ' ' -c "SELECT adopted_tree_digest, adopted_tar_digest FROM playbook_standing_launches WHERE id = '$WEBHOOK'")"
[ "$(jq -r .webhook.adopted_tree_digest "$WORK/webhook-created-W.json")" = "$W_TREE" ] ||
    fail "[W] the webhook names adopted tree $(jq -r .webhook.adopted_tree_digest "$WORK/webhook-created-W.json")"
W_REV=$(sql -c "SELECT rev FROM playbooks WHERE id = 'e2e-hook'")
sql -c "SELECT reason FROM events WHERE key LIKE '%$WEBHOOK'" | grep -qF "deliveries as revision $W_REV (tree $W_TREE)" ||
    fail "[W] the webhook's audit names: $(sql -c "SELECT reason FROM events WHERE key LIKE '%$WEBHOOK'")"
pass "[W] the webhook and its audit name the adopted tree beside the commit"

small_pack "$WREPO/pack" "test \"\$(cat marker.txt)\" = v2
$OK_JSON" '["check.sh", "marker.txt"]'
echo v2 >"$WREPO/pack/marker.txt"
printf '\n[outputs.gpu-capture]\ncount = 2\n' >>"$WREPO/pack/crucible.toml"
git_commit "$WREPO" v2
api POST /api/playbooks "{\"id\":\"e2e-hook\",\"description\":\"kind e2e\",$W_SOURCE}"
expect_http W 409 'declared exposure changed'
ACCEPT=$(jq -r .error "$WORK/api.json" | sed -E 's/.* to (sha256:[0-9a-f]+);.*/\1/')
api POST /api/playbooks "{\"id\":\"e2e-hook\",\"description\":\"kind e2e\",$W_SOURCE,\"accept_exposure_digest\":\"$ACCEPT\"}"
expect_http W 201 '"exposure_changed":true'
W_TREE2=$(playbook_tree e2e-hook)
is_tree "$W_TREE2" && [ "$W_TREE2" != "$W_TREE" ] || fail "[W] v2 registered with tree '$W_TREE2'"

watch_start W
[ "$(curl -s -o "$WORK/hook-W.json" -w '%{http_code}' -X POST -H 'content-type: application/json' --data '{"n":1}' "$HOOKS_URL/hooks/$WEBHOOK/$WEBHOOK_TOKEN")" = 202 ] ||
    fail "[W] the delivery was refused: $(cat "$WORK/hook-W.json")"
wait_for 60 "the webhook's launch" new_launch e2e-hook webhook ""
KEY=$(newest_launch e2e-hook webhook)
log "[W] delivery launched $KEY"
settle W "$KEY"
expect_finished W
delivered W "$KEY" none
watch_stop
[ "$LAUNCH_TREE" = "$W_TREE" ] || fail "[W] the firing ran $LAUNCH_TREE, not the adopted v1 tree"
[ "$(sql -c "SELECT digest FROM pack_tarballs WHERE issue_slug = '$(slug "$KEY")'")" = "$W_FOREIGN" ] ||
    fail "[W] the firing's pack row lost the adopted encoding's bytes or its tree"
read -r launched current <<<"$(sql -F ' ' -c "SELECT l.exposure_digest, p.exposure_digest FROM playbook_launches l JOIN playbooks p ON p.id = l.playbook WHERE l.key = '$KEY'")"
[ "$launched" = "$W_EXPOSURE" ] && [ "$current" != "$W_EXPOSURE" ] ||
    fail "[W] the firing recorded exposure '$launched' (v1 $W_EXPOSURE, registry now $current)"
RUN=$(jq -r '.runs[0].run_id' "$WORK/run-W.json")
crux graph "$RUN" --json >"$WORK/graph-W.json"
[ "$(jq -r '.outputs[] | select(.kind == "gpu-capture") | .count' "$WORK/graph-W.json")" = 1 ] ||
    fail "[W] the run graph serves outputs $(jq -c .outputs "$WORK/graph-W.json")"
pass "[W] the firing ran v1's tree from its adopted bytes and recorded and served v1's exposure"

log "[W] a one-shot against v2"
api POST /api/one-shots "{\"playbook\":\"e2e-hook\",\"max_cost\":1,\"max_time\":\"5m\",\"fire_at\":\"$(date -u -v+5S +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '+5 seconds' +%Y-%m-%dT%H:%M:%SZ)\"}"
expect_http W 201 '"playbook":"e2e-hook"'
watch_start W-once
wait_for 90 "the one-shot's launch" new_launch e2e-hook deferred ""
KEY=$(newest_launch e2e-hook deferred)
settle W-once "$KEY"
expect_finished W-once
delivered W-once "$KEY" none
watch_stop
[ "$LAUNCH_TREE" = "$W_TREE2" ] || fail "[W] the one-shot ran $LAUNCH_TREE, not v2's tree"
pass "[W] the one-shot ran v2's tree"

# ---- scenario S ----------------------------------------------------------------------------
log "[S] holding the only run slot while three launches wait for steering"
small_pack "$WORK/blocker" "sleep 30
$OK_JSON"
small_pack "$WORK/steer" "test \"\$(head -n 1 STEER.md)\" = frozen
grep -qx kind-steer-marker STEER.md
$OK_JSON" '["check.sh", "STEER.md"]'
echo frozen >"$WORK/steer/STEER.md"
small_pack "$WORK/heavysteer" "$OK_JSON"
for f in 1 2; do head -c 337500 /dev/urandom | base64 | tr -d '\n' >"$WORK/heavysteer/blob$f.txt"; done
small_pack "$WORK/bare" "grep -qx kind-steer-marker STEER.md
$OK_JSON" '["check.sh", "STEER.md"]'
for d in blocker steer heavysteer bare; do new_draft "$DRAFT-$d" "$WORK/$d"; done

watch_start S
launch S-block draft-launch "$DRAFT-blocker"
BLOCK="$KEY"
wait_for 120 "the blocker's pod to run" pods_in '.status.containerStatuses[]?.state.running' 1
launch S draft-launch "$DRAFT-steer"
STEERED="$KEY"
launch S-budget draft-launch "$DRAFT-heavysteer"
HEAVY_KEY="$KEY"
launch S-bare draft-launch "$DRAFT-bare"
BARE="$KEY"
for key in "$STEERED" "$HEAVY_KEY" "$BARE"; do
    [ "$(crux playbook-run "$key" | jq -r '.launch.status + " " + (.runs | length | tostring)')" = "new 0" ] ||
        fail "[S] $key dispatched while the blocker held the only slot"
done
insert_steering "$STEERED" 1 kind-steer-marker
insert_steering "$HEAVY_KEY" 1 "$(head -c 300000 /dev/urandom | base64 | tr -d '\n')"
insert_steering "$BARE" 1 kind-steer-marker
settle S-block "$BLOCK"
expect_finished S-block
settle S "$STEERED"
expect_finished S
settle S-budget "$HEAVY_KEY"
expect_parked S-budget "delivery budget"
settle S-bare "$BARE"
expect_finished S-bare
watch_stop
delivered S "$STEERED" inputs S
[ "$(tar -xzOf "$WORK/delivered-S.tar.gz" STEER.md)" = frozen ] || fail "[S] the pack key's STEER.md is not the frozen one"
[ "$(listing "$WORK/inputs-S.tar.gz")" = STEER.md ] || fail "[S] the inputs key lists $(listing "$WORK/inputs-S.tar.gz")"
[ "$(tar -xzOf "$WORK/inputs-S.tar.gz" STEER.md)" = "frozen
$STEER_STAMP
kind-steer-marker" ] || fail "[S] the inputs key's STEER.md is: $(tar -xzOf "$WORK/inputs-S.tar.gz" STEER.md)"
pass "[S] steering rode the inputs key over the frozen STEER.md, leaving the pack key the pack's own"
delivered S-bare "$BARE" inputs S
tree_paths "$LAUNCH_TREE" | grep -qx STEER.md && fail "[S] the bare pack ships a STEER.md"
pass "[S] an inject of STEER.md the pack does not ship is satisfied by the inputs key"
expect_no_pod S-budget "$HEAVY_KEY" S

# ---- scenario K ----------------------------------------------------------------------------
log "[K] firing schedules whose cursor files were advanced"
small_pack "$WORK/cursor" "test \"\$(cat cursor.json)\" = '{\"n\":7}'
$OK_JSON" '["check.sh", "cursor.json"]'
echo '{"n":0}' >"$WORK/cursor/cursor.json"
small_pack "$WORK/statecursor" "test \"\$(cat state/cursor.json)\" = '{\"n\":7}'
$OK_JSON" '["check.sh", "state/cursor.json"]'
small_pack "$WORK/runstate" "test \"\$(cat ../state/cursor.json)\" = '{\"n\":7}'
$OK_JSON"
for d in cursor statecursor runstate; do
    new_draft "$DRAFT-$d" "$WORK/$d"
    crux draft-publish "$DRAFT-$d" --playbook "k-$d" --json >"$WORK/publish-K-$d.json"
done

# schedule <playbook> <cursor path>: a yearly schedule with a run-file cursor; its id lands in
# SCHEDULE.
schedule() {
    api POST /api/schedules "{\"playbook\":\"$1\",\"cron_expr\":\"0 0 1 1 *\",\"max_cost\":1,\"max_time\":\"5m\",\"cursor\":{\"from\":\"check/out.txt\",\"path\":\"$2\"}}"
    expect_http K 201 "\"playbook\":\"$1\""
    SCHEDULE=$(jq -r .id "$WORK/api.json")
}

# fired <label> <playbook> <schedule> <mode>: leave the schedule's cursor where a finished firing
# would, make it due, and expect the launch it fires to succeed with that delivery.
fired() {
    local prev
    prev=$(newest_launch "$2" schedule)
    watch_start "$1"
    sql -c "UPDATE playbook_schedules SET cursor_value = '{\"n\":7}', next_due_at = '2000-01-01T00:00:00Z' WHERE id = '$3'"
    wait_for 60 "$2's scheduled launch" new_launch "$2" schedule "$prev"
    KEY=$(newest_launch "$2" schedule)
    settle "$1" "$KEY"
    expect_finished "$1"
    delivered "$1" "$KEY" "$4"
    watch_stop
}

api POST /api/schedules '{"playbook":"k-cursor","cron_expr":"0 0 1 1 *","max_cost":1,"max_time":"5m","cursor":{"from":"check/out.txt","path":"a\nb.json"}}'
expect_http K 422 '"field":"cursor.path"'
pass "[K] a cursor path that is not a pack path is refused at save"

schedule k-cursor cursor.json
[ "$(jq -r .adopted_tree_digest "$WORK/api.json")" = "$(playbook_tree k-cursor)" ] ||
    fail "[K] the schedule names adopted tree $(jq -r .adopted_tree_digest "$WORK/api.json")"
pass "[K] the schedule names the adopted tree_digest"
fired K k-cursor "$SCHEDULE" inputs
[ "$LAUNCH_TREE" = "$(playbook_tree k-cursor)" ] || fail "[K] the firing ran $LAUNCH_TREE, not the registered tree"
[ "$(listing "$WORK/inputs-K.tar.gz")" = cursor.json ] && [ "$(tar -xzOf "$WORK/inputs-K.tar.gz" cursor.json)" = '{"n":7}' ] ||
    fail "[K] the inputs key holds $(listing "$WORK/inputs-K.tar.gz")"
[ "$(tar -xzOf "$WORK/delivered-K.tar.gz" cursor.json)" = '{"n":0}' ] || fail "[K] the pack key's cursor.json is not the pack's default"
pass "[K] the cursor file rode the inputs key over the pack's default"

schedule k-statecursor state/cursor.json
fired K-state k-statecursor "$SCHEDULE" inputs
[ "$(listing "$WORK/inputs-K-state.tar.gz")" = state/cursor.json ] || fail "[K] the inputs key holds $(listing "$WORK/inputs-K-state.tar.gz")"
pass "[K] an injected cursor under state/ reached the run"

schedule k-runstate state/cursor.json
RUNSTATE_SCHEDULE="$SCHEDULE"
fired K-runstate k-runstate "$SCHEDULE" inputs
pass "[K] a cursor under state/ reached the run's state dir"

log "[K] firing a draft-head schedule"
api PUT /api/config/overrides '{"allow_draft_head_schedules":true,"justification":"kind e2e"}'
[ "$HTTP" = 200 ] || fail "[K] enabling draft-head schedules: $HTTP $(cat "$WORK/api.json")"
echo '# version 3' >>"$WORK/cursor/check.sh"
crux draft-push "$DRAFT-cursor" "$WORK/cursor" --base-version 2 --json >"$WORK/push-K-dh.json"
[ "$(jq -r .version "$WORK/push-K-dh.json")" = 3 ] || fail "[K] draft-push: $(cat "$WORK/push-K-dh.json")"
api POST /api/schedules "{\"playbook\":\"$DRAFT-cursor\",\"target_kind\":\"draft_head\",\"cron_expr\":\"0 0 1 1 *\",\"max_cost\":1,\"max_time\":\"5m\",\"cursor\":{\"from\":\"check/out.txt\",\"path\":\"cursor.json\"}}"
expect_http K 201 '"target_kind":"draft_head"'
HEAD_SCHEDULE=$(jq -r .id "$WORK/api.json")
[ "$(jq -r .adopted_tree_digest "$WORK/api.json")" = null ] || fail "[K] the draft-head schedule adopted a tree"
mkdir -p "$WORK/broken"
cp -R "$WORK/cursor/." "$WORK/broken/"
echo 'this is not starlark (' >"$WORK/broken/workflow.star"
crux draft-push "$DRAFT-cursor" "$WORK/broken" --base-version 3 --json >"$WORK/push-K-broken.json"
[ "$(jq -r .version "$WORK/push-K-broken.json")" = 4 ] || fail "[K] draft-push: $(cat "$WORK/push-K-broken.json")"
[ "$(sql -c "SELECT schema_digest IS NULL FROM playbook_draft_versions WHERE draft_id = '$DRAFT-cursor' AND version = 4")" = t ] ||
    fail "[K] version 4 compiled"
fired K-head "$DRAFT-cursor" "$HEAD_SCHEDULE" inputs
[ "$LAUNCH_TREE" = "$(draft_tree "$DRAFT-cursor" 3)" ] || fail "[K] the draft-head firing ran $LAUNCH_TREE, not version 3's tree"
pass "[K] the draft-head firing ran the newest compiled tree, not the newer save that does not compile"

stop_controller
log "[K] firing the state/ cursor schedule over a run-state claim"
kubectl -n "$NS" apply -f - >/dev/null <<'YAML'
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: crucible-e2e-state
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: standard
  resources:
    requests:
      storage: 64Mi
YAML
CONTROLLER_ENV=()
start_controller "$WORK/profile-state.toml"
fired K-pvc k-runstate "$RUNSTATE_SCHEDULE" inputs
pass "[K] a cursor under state/ reached the run over a run-state claim"

# ---- scenario X ----------------------------------------------------------------------------
log "[X] re-registering a playbook under authorized saves"
XREPO="$WORK/xrepo"
small_pack "$XREPO/pack" "echo x-0
$OK_JSON"
git -C "$XREPO" init -q
git_commit "$XREPO" x-0
X_SOURCE="\"repo\":\"file://$XREPO\",\"path\":\"pack\""
api POST /api/playbooks "{\"id\":\"e2e-x\",\"description\":\"kind e2e\",$X_SOURCE}"
expect_http X 201 '"id":"e2e-x"'
ONE_SHOT_AT=$(date -u -v+1d +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '+1 day' +%Y-%m-%dT%H:%M:%SZ)
# x_race <name> <path> <body> [method]: re-register e2e-x at a new commit while a save authorized
# against the old revision waits behind it on the registry row; the save is refused with 409.
x_race() {
    X_COMMITS=$((X_COMMITS + 1))
    printf 'echo x-%s\n' "$X_COMMITS" >>"$XREPO/pack/check.sh"
    git_commit "$XREPO" "x-$X_COMMITS"
    hold_lock "SELECT 1 FROM playbooks WHERE id = 'e2e-x' FOR UPDATE"
    api_bg "register-$1" POST /api/playbooks "{\"id\":\"e2e-x\",\"description\":\"kind e2e\",$X_SOURCE}"
    wait_for 60 "the re-registration to wait on the row" lock_waiters 1
    api_bg "$1" "${4:-POST}" "$2" "$3"
    wait_for 60 "the $1 save to wait behind it" lock_waiters 2
    release_lock
    api_wait
    expect_bg X "register-$1" 201 '"id":"e2e-x"'
    expect_bg X "$1" 409 'was re-registered at revision'
}
api POST /api/schedules '{"playbook":"e2e-x","cron_expr":"0 0 1 1 *","max_cost":1,"max_time":"5m"}'
expect_http X 201 '"playbook":"e2e-x"'
X_SCHEDULE=$(jq -r .id "$WORK/api.json")
X_ADOPTED=$(sql -c "SELECT adopted_tree_digest FROM playbook_standing_launches WHERE id = '$X_SCHEDULE'")
[ "$X_ADOPTED" = "$(playbook_tree e2e-x)" ] || fail "[X] the schedule adopted '$X_ADOPTED'"
X_REV=$(sql -c "SELECT rev FROM playbooks WHERE id = 'e2e-x'")
sql -c "SELECT reason FROM events WHERE key = 'schedule:$X_SCHEDULE'" | grep -qF "as revision $X_REV (tree $X_ADOPTED)" ||
    fail "[X] the schedule's audit names: $(sql -c "SELECT reason FROM events WHERE key = 'schedule:$X_SCHEDULE'")"
pass "[X] the schedule's audit names the adopted tree beside the commit"
X_COMMITS=0
x_race schedule-edit "/api/schedules/$X_SCHEDULE" '{"playbook":"e2e-x","cron_expr":"0 0 2 1 *","max_cost":1,"max_time":"5m"}' PUT
x_race launch /api/playbooks/e2e-x/launch '{"max_cost":1,"max_time":"5m"}'
x_race schedule /api/schedules '{"playbook":"e2e-x","cron_expr":"0 0 1 1 *","max_cost":1,"max_time":"5m"}'
x_race webhook /api/webhooks '{"playbook":"e2e-x","verifier":"path_token","dedupe":"string(body.n)","max_launches_per_hour":1,"max_cost":1,"max_time":"5m"}'
x_race one-shot /api/one-shots "{\"playbook\":\"e2e-x\",\"max_cost\":1,\"max_time\":\"5m\",\"fire_at\":\"$ONE_SHOT_AT\"}"
[ "$(sql -c "SELECT count(*) FROM playbook_launches WHERE playbook = 'e2e-x'")" = 0 ] ||
    fail "[X] a refused launch was stored"
[ "$(sql -F ' ' -c "SELECT id, cron_expr, adopted_tree_digest FROM playbook_standing_launches s LEFT JOIN playbook_schedules USING (id) WHERE s.playbook = 'e2e-x'")" = "$X_SCHEDULE 0 0 1 1 * $X_ADOPTED" ] ||
    fail "[X] a refused save stored or changed a standing launch: $(sql -c "SELECT * FROM playbook_standing_launches WHERE playbook = 'e2e-x'")"
pass "[X] a schedule edit, launch, schedule, webhook, and one-shot authorized before a re-registration are refused with 409"
X_TREE=$(playbook_tree e2e-x)
X_REV2=$(sql -c "SELECT rev FROM playbooks WHERE id = 'e2e-x'")
api PUT "/api/schedules/$X_SCHEDULE" '{"playbook":"e2e-x","cron_expr":"0 0 3 1 *","max_cost":1,"max_time":"5m"}'
expect_http X 200 "\"adopted_tree_digest\":\"$X_TREE\""
sql -c "SELECT reason FROM events WHERE key = 'schedule:$X_SCHEDULE'" |
    grep -qF "from revision $X_REV (tree $X_ADOPTED) to revision $X_REV2 (tree $X_TREE)" ||
    fail "[X] the schedule edit's audit names: $(sql -c "SELECT reason FROM events WHERE key = 'schedule:$X_SCHEDULE'")"
pass "[X] a schedule edit adopts the current tree, and its audit names both trees beside their commits"

api POST /api/playbooks "{\"id\":\"e2e-xgone\",\"description\":\"kind e2e\",$X_SOURCE}"
expect_http X 201 '"id":"e2e-xgone"'
hold_lock "DELETE FROM playbooks WHERE id = 'e2e-xgone'"
api_bg gone POST /api/schedules '{"playbook":"e2e-xgone","cron_expr":"0 0 1 1 *","max_cost":1,"max_time":"5m"}'
wait_for 60 "the schedule save to wait on the deleted row" lock_waiters 1
release_lock
api_wait
expect_bg X gone 404 'was deregistered while the save was being authorized'
[ "$(sql -c "SELECT count(*) FROM playbook_standing_launches WHERE playbook = 'e2e-xgone'")" = 0 ] ||
    fail "[X] the refused schedule was stored"
pass "[X] a schedule authorized before its playbook was deleted is refused with 404"

# ---- scenario Q ----------------------------------------------------------------------------
stop_controller
CONTROLLER_BIN="$AUTORESEARCH_CONTROLLER"
CONTROLLER_ENV=(CONTROLLER_AUTORESEARCH=true CONTROLLER_DAILY_COST_CEILING_USD=0)
start_controller
log "[Q] launching a direct autoresearch pack while a zero daily ceiling holds its run"
QREPO="$WORK/qrepo"
mkdir -p "$QREPO"
cp -R "$FIX/packs/scope" "$QREPO/pack"
git -C "$QREPO" init -q
git_commit "$QREPO" scope
api POST /api/packs/launch "{\"repo\":\"file://$QREPO\",\"path\":\"pack\",\"justification\":\"kind e2e\"}"
expect_http Q 201 '"status":"awaiting-approval"'
SCOPE_KEY=$(jq -r .key "$WORK/api.json")
SCOPE_TREE=$(sql -c "SELECT tree_digest FROM scopes WHERE issue = '$SCOPE_KEY'")
is_tree "$SCOPE_TREE" && [ "$SCOPE_TREE" = "$(launch_tree "$SCOPE_KEY")" ] ||
    fail "[Q] the scope froze '$SCOPE_TREE'; its pack row holds '$(launch_tree "$SCOPE_KEY")'"
pass "[Q] the approved scope froze the tree its launch stored"
api GET "/api/approvals/$(sql -c "SELECT id FROM scopes WHERE issue = '$SCOPE_KEY'")/evidence"
[ "$HTTP" = 200 ] && [ "$(jq -c '[.rounds[] | [.round, .kind, .outcome.result]]' "$WORK/api.json")" = '[[1,"propose","passed"]]' ] ||
    fail "[Q] the approval evidence: $HTTP $(cat "$WORK/api.json")"
pass "[Q] the approval evidence reads SCOPE.md from the stored tree"

log "[Q] writing a tree that declares a build to the scope's pack row"
cp -R "$FIX/packs/scope" "$WORK/scopedecoy"
printf '\n[build.sandbox]\nbackend = "cluster"\nimage = "ghcr.io/org/sandbox"\ntimeout = "30m"\n[build.sandbox.cluster]\ncontainerfile = "Containerfile"\n' >>"$WORK/scopedecoy/crucible.toml"
echo 'FROM scratch' >"$WORK/scopedecoy/Containerfile"
new_draft "$DRAFT-scopedecoy" "$WORK/scopedecoy"
SCOPE_DECOY=$(draft_tree "$DRAFT-scopedecoy" 2)
is_tree "$SCOPE_DECOY" && [ "$SCOPE_DECOY" != "$SCOPE_TREE" ] || fail "[Q] the decoy saved as tree '$SCOPE_DECOY'"
sql -c "UPDATE pack_tarballs SET tree_digest = '$SCOPE_DECOY' WHERE issue_slug = '$(slug "$SCOPE_KEY")'"
[ "$(sql -c "SELECT status FROM issues WHERE key = '$SCOPE_KEY'")" = awaiting-approval ] ||
    fail "[Q] the scope moved while the ceiling held it: $(sql -c "SELECT status FROM issues WHERE key = '$SCOPE_KEY'")"
watch_start Q
api PUT /api/config/overrides '{"daily_cost_ceiling":100,"justification":"kind e2e"}'
[ "$HTTP" = 200 ] || fail "[Q] raising the daily ceiling: $HTTP $(cat "$WORK/api.json")"
crux reconcile >/dev/null
wait_for 120 "the scope's run pod to stage its pack" staged Q "$SCOPE_KEY"
watch_stop
delivered Q "$SCOPE_KEY" none Q "$SCOPE_TREE"
[ "$(sql -c "SELECT count(*) FROM events WHERE key = '$SCOPE_KEY' AND to_status = 'building'")" = 0 ] ||
    fail "[Q] the scope planned the decoy's build"
[ "$(sql -c "SELECT count(*) FROM events WHERE key = '$SCOPE_KEY' AND from_status = 'awaiting-approval' AND to_status = 'running'")" = 1 ] ||
    fail "[Q] the scope did not launch its run: $(sql -c "SELECT from_status, to_status, reason FROM events WHERE key = '$SCOPE_KEY'")"
pass "[Q] the scope planned and delivered its frozen tree, not the tree written to its pack row"

# ---- scenario O ----------------------------------------------------------------------------
stop_controller
CONTROLLER_BIN="$BIN/crucible-controller"
CONTROLLER_ENV=(CONTROLLER_PLAYBOOK_EXECUTOR=local CONTROLLER_MAX_CONCURRENT_PODS=1)
start_controller
log "[O] running stored trees with the local executor"
small_pack "$WORK/localblock" "sleep 20
$OK_JSON"
small_pack "$WORK/localsteer" "echo local-$ID
test \"\$(head -n 1 STEER.md)\" = frozen
grep -qx kind-steer-marker STEER.md
$OK_JSON" '["check.sh", "STEER.md"]'
echo frozen >"$WORK/localsteer/STEER.md"
new_draft "$DRAFT-lblock" "$WORK/localblock"
new_draft "$DRAFT-lsteer" "$WORK/localsteer"
watch_start O
launch O-block draft-launch "$DRAFT-lblock"
BLOCK="$KEY"
wait_for 60 "the local blocker to run" is_running "$BLOCK"
launch O draft-launch "$DRAFT-lsteer"
LOCAL_KEY="$KEY"
[ "$(crux playbook-run "$LOCAL_KEY" | jq -r '.launch.status + " " + (.runs | length | tostring)')" = "new 0" ] ||
    fail "[O] $LOCAL_KEY dispatched while the blocker held the only slot"
insert_steering "$LOCAL_KEY" 1 kind-steer-marker
settle O-block "$BLOCK"
expect_finished O-block
settle O "$LOCAL_KEY"
expect_finished O
small_pack "$WORK/localtamper" "echo local-tamper-$ID
$OK_JSON"
new_draft "$DRAFT-ltamper" "$WORK/localtamper"
crux draft-publish "$DRAFT-ltamper" --playbook "$DRAFT-ltpub" --json >"$WORK/publish-O.json"
LOCAL_TAMPER=$(playbook_tree "$DRAFT-ltpub")
sql -c "UPDATE pack_tree_files SET content = 'tampered' WHERE digest = '$LOCAL_TAMPER' AND path = 'check.sh'"
launch O-tamper launch "$DRAFT-ltpub"
settle O-tamper "$KEY"
watch_stop
expect_parked O-tamper "$LOCAL_TAMPER no longer matches its stored files"
for key in "$BLOCK" "$LOCAL_KEY" "$KEY"; do
    [ "$(pods_seen O "$key")" = 0 ] || fail "[O] the local executor created a pod for $key"
done
LOCAL_PACK=$(ls -d "$WORK/scratch/local-runs/$(slug "$LOCAL_KEY")"-*/pack)
[ "$(cat "$LOCAL_PACK/STEER.md")" = "frozen
$STEER_STAMP
kind-steer-marker" ] || fail "[O] the local run's STEER.md is: $(cat "$LOCAL_PACK/STEER.md")"
pass "[O] the local executor ran the stored tree with its steering and parked the tampered one"

# ---- scenario V ----------------------------------------------------------------------------
log "[V] pinning a binding and a fork to a draft-sourced playbook, and readying unconvertible rows"
small_pack "$WORK/vpin" "$OK_JSON"
new_draft "$DRAFT-vpin" "$WORK/vpin"
crux draft-publish "$DRAFT-vpin" --playbook e2e-vpub --json >"$WORK/publish-V.json"
V_TREE=$(playbook_tree e2e-vpub)
is_tree "$V_TREE" || fail "[V] e2e-vpub registered with tree '$V_TREE'"
api POST /api/secrets '{"name":"vpin_token","kind":"opaque","mint":"github-app"}'
expect_http V 201 '"name":"vpin_token"'
V_SECRET=$(jq -r .id "$WORK/api.json")

# bind_vpin <pack_rev>: bind vpin_token to e2e-vpub, reviewed against that revision.
bind_vpin() {
    api POST "/api/secrets/$V_SECRET/bindings" "{\"scope_kind\":\"playbook\",\"scope_id\":\"e2e-vpub\",\"projection_kind\":\"env\",\"projection\":\"VPIN_TOKEN\",\"pack_rev\":\"$1\"}"
}

# v_pins: e2e-vpub's rev and tree, the binding's rev and tree, and the fork's rev and tree.
v_pins() {
    sql -F ' ' -c "SELECT p.rev, p.tree_digest, b.pack_rev, b.pack_digest, d.origin_rev, d.origin_digest
                   FROM playbooks p, secret_bindings b, playbook_drafts d
                   WHERE p.id = 'e2e-vpub' AND b.id = '$V_BINDING' AND d.id = 'e2e-vfork'"
}

# expect_released <label> <playbook>: a launch of the playbook resolves its binding, which the
# local executor refuses to run without: the binding was not stale.
expect_released() {
    launch "$1" launch "$2"
    settle "$1" "$KEY"
    expect_parked "$1" "this launch resolved 1 binding(s)"
}

bind_vpin "$V_TREE"
expect_http V 201 '"scope_id":"e2e-vpub"'
V_BINDING=$(jq -r .id "$WORK/api.json")
[ "$(sql -c "SELECT pack_digest FROM secret_bindings WHERE id = '$V_BINDING'")" = "$V_TREE" ] ||
    fail "[V] the binding pinned '$(sql -c "SELECT pack_digest FROM secret_bindings WHERE id = '$V_BINDING'")'"
clone_template V e2e-vfork e2e-vpub "$V_TREE"
[ "$(sql -c "SELECT origin_digest FROM playbook_drafts WHERE id = 'e2e-vfork'")" = "$V_TREE" ] ||
    fail "[V] the fork pinned origin '$(sql -c "SELECT origin_digest FROM playbook_drafts WHERE id = 'e2e-vfork'")'"
pass "[V] the binding and the fork pin e2e-vpub's tree"

VREPO="$WORK/vrepo"
small_pack "$VREPO/pack" "echo vgit-$ID
$OK_JSON"
git -C "$VREPO" init -q
git_commit "$VREPO" pack
api POST /api/playbooks "{\"id\":\"e2e-vgit\",\"description\":\"kind e2e\",\"repo\":\"file://$VREPO\",\"path\":\"pack\"}"
expect_http V 201 '"id":"e2e-vgit"'

small_pack "$WORK/vuncv" "echo uncv-$ID
$OK_JSON"
new_draft "$DRAFT-vuncv" "$WORK/vuncv"
crux draft-publish "$DRAFT-vuncv" --playbook e2e-vuncvpub --json >"$WORK/publish-V-uncv.json"
api POST /api/schedules '{"playbook":"e2e-vuncvpub","cron_expr":"0 0 1 1 *","max_cost":1,"max_time":"5m"}'
expect_http V 201 '"playbook":"e2e-vuncvpub"'
V_SCHEDULE=$(jq -r .id "$WORK/api.json")
api PUT /api/config/overrides '{"allow_draft_head_schedules":true,"justification":"kind e2e"}'
[ "$HTTP" = 200 ] || fail "[V] enabling draft-head schedules: $HTTP $(cat "$WORK/api.json")"
api POST /api/schedules "{\"playbook\":\"$DRAFT-vuncv\",\"target_kind\":\"draft_head\",\"cron_expr\":\"0 0 1 1 *\",\"max_cost\":1,\"max_time\":\"5m\"}"
expect_http V 201 '"target_kind":"draft_head"'
V_HEAD_SCHEDULE=$(jq -r .id "$WORK/api.json")
api POST /api/playbooks/imports "{\"repo\":\"file://$VREPO\",\"path\":\"pack\"}"
expect_http V 201 '"status"'
V_IMPORT=$(jq -r .id "$WORK/api.json")
VWREPO="$WORK/vwrepo"
small_pack "$VWREPO/pack" "echo watch-$ID
$OK_JSON"
printf 'params = {"key": {"type": "string", "required": True}}\n\n%s\n' "$(cat "$FIX/packs/deliver/workflow.star")" >"$VWREPO/pack/workflow.star"
git -C "$VWREPO" init -q
git_commit "$VWREPO" watch
api POST /api/playbooks "{\"id\":\"e2e-vwatch\",\"description\":\"kind e2e\",\"repo\":\"file://$VWREPO\",\"path\":\"pack\"}"
expect_http V 201 '"id":"e2e-vwatch"'
small_pack "$WORK/vdisp" "$OK_JSON"
new_draft "$DRAFT-vdisp" "$WORK/vdisp"

log "[V] encoding scope packs with the engine"
VCODE="$WORK/vcode"
mkdir -p "$VCODE"
echo code >"$VCODE/README"
git -C "$VCODE" init -q
git_commit "$VCODE" code

# scope_pack <dir>: an autoresearch pack the engine's scope pipeline freezes with no agent turn.
scope_pack() {
    mkdir -p "$1"
    cp "$FIX/packs/scope/workflow.star" "$1/"
    cat >"$1/crucible.toml" <<TOML
[repo]
path = "$VCODE"

[workspace]
inject = ["measure.sh"]

[agent]
backend = "openshell"
goal = "Freeze the pack."
sandbox_image = "ghcr.io/org/sandbox:latest"

[judge]
measure_cmd = "./measure.sh"
direction = "higher"
objective = "score"

[workflow]
type = "autoresearch"
file = "workflow.star"
TOML
    printf '#!/bin/sh\necho %s\n' "'{\"valid\": true, \"score\": 1, \"pass\": true}'" >"$1/measure.sh"
    chmod +x "$1/measure.sh"
}

# scope_marker <dir>: the pack marker payload a surviving scope of the dir emits.
scope_marker() {
    "$AUTORESEARCH_ENGINE" scope --pack "$1" --json --marker >"$1.out" 2>"$1.err" ||
        fail "[V] the scope of $1 did not survive: $(grep CRUCIBLE_SCOPE_REPORT "$1.out")"
    sed -n 's/^CRUCIBLE_SCOPE_PACK: //p' "$1.out"
}

scope_pack "$WORK/vscope"
mkdir -p "$WORK/vscope/state" "$WORK/vscope/workspace" "$WORK/vscope/.git" "$WORK/vscope/sub/state"
for f in state/cursor workspace/main.go .git/HEAD sub/state/x; do echo junk >"$WORK/vscope/$f"; done
ln -s /etc/passwd "$WORK/vscope/state/link"
echo kept >"$WORK/vscope/sub/kept.txt"
scope_marker "$WORK/vscope" | base64 -d >"$WORK/vscope.tar.gz"
V_SCOPE_FILES="SCOPE.md WORKFLOW.png crucible.toml measure.sh sub/kept.txt workflow.star "
[ "$(tar -tzf "$WORK/vscope.tar.gz" | tr '\n' ' ')" = "$V_SCOPE_FILES" ] ||
    fail "[V] the engine's scope pack lists $(tar -tzf "$WORK/vscope.tar.gz" | tr '\n' ' ')"
pass "[V] the engine's scope pack skips state/, workspace/ and .git at any depth and lists its entries in order"
scope_pack "$WORK/vscopelink"
ln -s crucible.toml "$WORK/vscopelink/link"
scope_marker "$WORK/vscopelink" >"$WORK/vscopelink.marker"
jq -r .error "$WORK/vscopelink.marker" | grep -q 'link is a symbolic link' ||
    fail "[V] a scope pack holding a symlink emitted: $(cut -c1-300 "$WORK/vscopelink.marker")"
scope_pack "$WORK/vscopebig"
head -c 70000000 /dev/zero >"$WORK/vscopebig/zeros"
scope_marker "$WORK/vscopebig" >"$WORK/vscopebig.marker"
rm "$WORK/vscopebig/zeros"
jq -r .error "$WORK/vscopebig.marker" | grep -Eq "pack tar is [0-9]+ bytes, over the controller's 67108864-byte expanded cap" ||
    fail "[V] an oversize scope pack emitted: $(cut -c1-300 "$WORK/vscopebig.marker")"
pass "[V] the engine refuses a scope pack holding a symlink, or past the controller's expanded cap, naming why"

stop_controller
log "[V] rewriting e2e-vpub, its binding, and its fork as a controller from before trees left them"
host_tarball "$WORK/vpin" "$WORK/vpin.tar.gz"
sql_file "$WORK/vpin.tar.gz" <<'SQL'
UPDATE playbooks
SET tar_gz = s.b, tar_digest = 'sha256:' || encode(sha256(s.b), 'hex'), tar_bytes = length(s.b),
    rev = 'sha256:' || encode(sha256(s.b), 'hex'), tree_digest = NULL
FROM (SELECT decode(:'hex', 'hex') AS b) s
WHERE id = 'e2e-vpub';
SQL
V_OLD="sha256:$(sha256 "$WORK/vpin.tar.gz")"
sql -c "UPDATE secret_bindings SET pack_rev = '$V_OLD', pack_digest = NULL WHERE id = '$V_BINDING'"
sql -c "UPDATE playbook_drafts SET origin_rev = '$V_OLD', origin_digest = NULL WHERE id = 'e2e-vfork'"
[ "$(v_pins)" = "$V_OLD  $V_OLD  $V_OLD " ] || fail "[V] the pre-tree rows read $(v_pins)"
mkdir -p "$WORK/vsym" "$WORK/vgc"
echo m >"$WORK/vsym/crucible.toml"
ln -s crucible.toml "$WORK/vsym/link"
host_tarball "$WORK/vsym" "$WORK/vsym.tar.gz"
seed_pack_row kind_e2e_vsym "$WORK/vsym.tar.gz"
echo "collected $ID" >"$WORK/vgc/crucible.toml"
host_tarball "$WORK/vgc" "$WORK/vgc.tar.gz"
seed_pack_row kind_e2e_vgc "$WORK/vgc.tar.gz"
seed_pack_row kind_e2e_vscope "$WORK/vscope.tar.gz"

CONTROLLER_ENV=(CONTROLLER_PLAYBOOK_EXECUTOR=local CONTROLLER_MAX_CONCURRENT_PODS=1 CONTROLLER_SCHEDULE_AUTO_DISABLE_FAILURES=1
    JIRA_BASE_URL=http://127.0.0.1:9 JIRA_EMAIL=e2e@example.com JIRA_API_TOKEN=e2e)
start_controller
read -r converted unconvertible <<<"$(conversion_report "$BOOT")"
[ "$converted $unconvertible $(pinned_report "$BOOT")" = "3 1 3" ] ||
    fail "[V] boot $BOOT reported converted/unconvertible/pinned '$converted $unconvertible $(pinned_report "$BOOT")'; expected 3 1 3"
[ "$(v_pins)" = "$V_TREE $V_TREE $V_OLD $V_TREE $V_OLD $V_TREE" ] ||
    fail "[V] conversion left the pins at $(v_pins)"
[ "$(sql -c "SELECT tree_digest FROM pack_digest_aliases WHERE old_digest = '$V_OLD'")" = "$V_TREE" ] ||
    fail "[V] the old rev is not an alias of $V_TREE"
pass "[V] conversion made the draft-sourced rev its tree and pinned the binding and the fork to it, in one boot"
api GET /api/playbook-drafts/e2e-vfork
[ "$(jq -c '.origin | [.rev, .current_rev, .digest, .current_digest, .moved]' "$WORK/api.json")" = "[\"$V_OLD\",\"$V_TREE\",\"$V_TREE\",\"$V_TREE\",false]" ] ||
    fail "[V] the fork's origin reads $(jq -c .origin "$WORK/api.json")"
pass "[V] the fork whose rev names the old bytes has not moved: its origin digest is the current tree"
expect_released V-pin e2e-vpub

V_SCOPE_TREE=$(launch_tree kind_e2e_vscope)
is_tree "$V_SCOPE_TREE" && [ "$(tree_tarball "$V_SCOPE_TREE")" = "sha256:$(sha256 "$WORK/vscope.tar.gz")" ] ||
    fail "[V] the engine's scope pack converted to '$V_SCOPE_TREE', whose tarball is $(tree_tarball "$V_SCOPE_TREE"), not the engine's bytes"
[ "$(tree_paths "$V_SCOPE_TREE" | tr '\n' ' ')" = "$V_SCOPE_FILES" ] ||
    fail "[V] the engine's scope pack stored $(tree_paths "$V_SCOPE_TREE" | tr '\n' ' ')"
pass "[V] the controller reads the engine's scope pack back as a tree whose canonical tarball is the engine's bytes"

log "[V] supplying the pre-tree digest as a template pin and as a binding's revision"
V_UNKNOWN="sha256:$(printf '0%.0s' $(seq 1 64))"
# template_v <label> <draft> <playbook> <json pins>: template a draft from the playbook at the
# pins; the answer lands in template-<label>.json and .code.
template_v() {
    api POST /api/playbook-drafts "$(jq -nc --arg id "$2" --arg t "$3" --argjson pins "$4" \
        '{id: $id, description: "kind e2e", template: $t} + $pins')"
    cp "$WORK/api.json" "$WORK/template-$1.json"
    echo "$HTTP" >"$WORK/template-$1.code"
}
template_v sup-rev e2e-vsup e2e-vpub "{\"template_rev\":\"$V_OLD\"}"
expect_http V 409 "revision $V_OLD is superseded by $V_TREE"
template_v sup-digest e2e-vsup e2e-vpub "{\"template_rev\":\"$V_TREE\",\"template_digest\":\"$V_OLD\"}"
expect_http V 409 "revision $V_OLD is superseded by $V_TREE"
template_v unknown e2e-vsup e2e-vpub "{\"template_rev\":\"$V_TREE\",\"template_digest\":\"$V_UNKNOWN\"}"
expect_http V 409 'moved while cloning'
V_GIT_REV=$(sql -c "SELECT rev FROM playbooks WHERE id = 'e2e-vgit'")
template_v other-old e2e-vsup e2e-vgit "{\"template_rev\":\"$V_GIT_REV\",\"template_digest\":\"$V_OLD\"}"
template_v other-unknown e2e-vsup e2e-vgit "{\"template_rev\":\"$V_GIT_REV\",\"template_digest\":\"$V_UNKNOWN\"}"
{ [ "$(cat "$WORK/template-other-old.code")" = 409 ] && cmp -s "$WORK/template-other-old.json" "$WORK/template-other-unknown.json"; } ||
    fail "[V] a playbook that never held $V_TREE answered the old digest $(cat "$WORK/template-other-old.code") $(cat "$WORK/template-other-old.json"), the unknown one $(cat "$WORK/template-other-unknown.json")"
[ -z "$(sql -c "SELECT id FROM playbook_drafts WHERE id = 'e2e-vsup'")" ] || fail "[V] a refused template created e2e-vsup"
bind_vpin "$V_OLD"
expect_http V 409 "pack revision $V_OLD is superseded by $V_TREE"
pass "[V] a reader supplying the pre-tree digest is told its replacement; a playbook that never held it answers as for an unknown digest"

log "[V] watching a tracker query from a git-sourced playbook"
VW_TREE=$(playbook_tree e2e-vwatch)
VW_REV=$(sql -c "SELECT rev FROM playbooks WHERE id = 'e2e-vwatch'")
VW_BODY='{"playbook":"e2e-vwatch","tracker":"jira","query":"project = E2E","key_param":"key","max_cost":1,"max_time":"5m","enabled":false}'
api POST /api/watches "$VW_BODY"
expect_http V 201 "\"adopted_tree_digest\":\"$VW_TREE\""
VW_ID=$(jq -r .id "$WORK/api.json")
api PUT "/api/watches/$VW_ID" "$VW_BODY"
expect_http V 200 "\"adopted_tree_digest\":\"$VW_TREE\""
[ "$(sql -c "SELECT reason FROM events WHERE key LIKE '%$VW_ID'" | grep -cF "as revision $VW_REV (tree $VW_TREE)")" = 2 ] ||
    fail "[V] the watch's audits name: $(sql -c "SELECT reason FROM events WHERE key LIKE '%$VW_ID'")"
pass "[V] a watch names its adopted tree_digest, and its create and edit audits name the tree beside the commit"

log "[V] refusing unconvertible packs naming the reason"
V_SYM_DIGEST="sha256:$(sha256 "$WORK/vsym.tar.gz")"
V_SYM_REASON=$(sql -c "SELECT unconvertible_reason FROM pack_digest_aliases WHERE old_digest = '$V_SYM_DIGEST' AND tree_digest IS NULL")
grep -q 'symbolic link' <<<"$V_SYM_REASON" || fail "[V] the symlink pack is recorded unconvertible as '$V_SYM_REASON'"
sql -v draft="$DRAFT-vuncv" -v import="$V_IMPORT" -v schedule="$V_SCHEDULE" <<'SQL'
CREATE TEMP VIEW sym AS SELECT tar_gz, digest, bytes FROM pack_tarballs WHERE issue_slug = 'kind_e2e_vsym';
UPDATE playbook_draft_versions SET tar_gz = sym.tar_gz, tar_digest = sym.digest, tar_bytes = sym.bytes
FROM sym WHERE draft_id = :'draft' AND version = 2;
UPDATE playbooks SET tar_gz = sym.tar_gz, tar_digest = sym.digest, tar_bytes = sym.bytes
FROM sym WHERE id = 'e2e-vuncvpub';
UPDATE pack_imports SET tar_gz = sym.tar_gz, tar_digest = sym.digest, tar_bytes = sym.bytes
FROM sym WHERE id = :'import';
UPDATE playbook_standing_launches
SET adopted_tar_gz = sym.tar_gz, adopted_tar_digest = sym.digest, adopted_tar_bytes = sym.bytes
FROM sym WHERE id = :'schedule';
SQL
[ "$(sql -c "SELECT count(*) FROM playbook_draft_versions WHERE draft_id = '$DRAFT-vuncv' AND version = 2 AND tree_digest IS NULL")
$(sql -c "SELECT count(*) FROM playbooks WHERE id = 'e2e-vuncvpub' AND tree_digest IS NULL")
$(sql -c "SELECT count(*) FROM pack_imports WHERE id = '$V_IMPORT' AND tree_digest IS NULL")
$(sql -c "SELECT count(*) FROM playbook_standing_launches WHERE id = '$V_SCHEDULE' AND adopted_tree_digest IS NULL")" = "1
1
1
1" ] || fail "[V] a row kept its tree when its bytes became the symlink pack"
if crux draft-launch "$DRAFT-vuncv" --max-cost 1 --max-time 5m >"$WORK/launch-V-uncv.log" 2>&1; then
    fail "[V] an unconvertible draft version launched"
fi
grep -qF "$V_SYM_REASON" "$WORK/launch-V-uncv.log" || fail "[V] the draft launch refusal: $(cat "$WORK/launch-V-uncv.log")"
api POST /api/playbook-drafts '{"id":"e2e-vuncvclone","description":"kind e2e","template":"e2e-vuncvpub"}'
expect_http V 422 "$V_SYM_REASON"
api POST "/api/playbooks/imports/$V_IMPORT/draft" '{"id":"e2e-vuncvopen","description":"kind e2e"}'
expect_http V 422 "$V_SYM_REASON"
[ "$(sql -c "SELECT count(*) FROM playbook_drafts WHERE id IN ('e2e-vuncvclone', 'e2e-vuncvopen')")" = 0 ] ||
    fail "[V] a refused clone or open left a draft"
pass "[V] a draft launch, a template clone, and opening an import as a draft refuse the unconvertible pack naming the reason"

# disabled_for <schedule>: the schedule is disabled.
disabled_for() { [ "$(sql -c "SELECT enabled FROM playbook_standing_launches WHERE id = '$1'")" = f ]; }
sql -c "UPDATE playbook_schedules SET next_due_at = '2000-01-01T00:00:00Z' WHERE id IN ('$V_SCHEDULE', '$V_HEAD_SCHEDULE')"
for schedule in "$V_SCHEDULE" "$V_HEAD_SCHEDULE"; do
    wait_for 60 "schedule $schedule to fail its firing" disabled_for "$schedule"
    sql -c "SELECT reason FROM events WHERE key = 'schedule:$schedule' AND reason LIKE 'auto-disabled after 1 consecutive%'" |
        grep -qF "$V_SYM_REASON" ||
        fail "[V] schedule $schedule was disabled for: $(sql -c "SELECT reason FROM events WHERE key = 'schedule:$schedule'")"
done
[ "$(sql -c "SELECT count(*) FROM playbook_launches WHERE playbook IN ('e2e-vuncvpub', '$DRAFT-vuncv')")" = 0 ] ||
    fail "[V] a refused firing launched"
pass "[V] an adopted and a draft-head schedule fail their firing naming the reason and count toward auto-disable"

# hold_dispatch / release_dispatch: a zero daily ceiling keeps every new launch from dispatching.
hold_dispatch() {
    api PUT /api/config/overrides '{"daily_cost_ceiling":0,"justification":"kind e2e"}'
    [ "$HTTP" = 200 ] || fail "[V] zeroing the daily ceiling: $HTTP $(cat "$WORK/api.json")"
}
release_dispatch() {
    api PUT /api/config/overrides '{"daily_cost_ceiling":100,"justification":"kind e2e"}'
    [ "$HTTP" = 200 ] || fail "[V] raising the daily ceiling: $HTTP $(cat "$WORK/api.json")"
    crux reconcile >/dev/null
}
# held <key>: the launch is still new, with no run.
held() {
    crux reconcile >/dev/null
    [ "$(crux playbook-run "$1" | jq -r '.launch.status + " " + (.runs | length | tostring)')" = "new 0" ] ||
        fail "[V] $1 dispatched while the ceiling held it"
}

hold_dispatch
launch V-disp draft-launch "$DRAFT-vdisp"
V_DISP="$KEY"
held "$V_DISP"
sql -v slug="$(slug "$V_DISP")" <<'SQL'
UPDATE pack_tarballs SET tar_gz = s.tar_gz, digest = s.digest, bytes = s.bytes
FROM (SELECT tar_gz, digest, bytes FROM pack_tarballs WHERE issue_slug = 'kind_e2e_vsym') s
WHERE issue_slug = :'slug';
SQL
[ -z "$(launch_tree "$V_DISP")" ] || fail "[V] the queued launch kept its tree"
release_dispatch
settle V-disp "$V_DISP"
expect_parked V-disp "pack $V_SYM_DIGEST is unconvertible: $V_SYM_REASON"
[ "$(jq '.runs | length' "$WORK/run-V-disp.json")" = 0 ] || fail "[V] the unconvertible launch recorded a run"

log "[V] unpinning one old tree and keeping a pinned and a fresh one"
V_GC_TREE=$(launch_tree kind_e2e_vgc)
is_tree "$V_GC_TREE" || fail "[V] the collectable pack converted to '$V_GC_TREE'"
sql -c "DELETE FROM pack_tarballs WHERE issue_slug = 'kind_e2e_vgc'"
small_pack "$WORK/vfresh" "echo fresh-$ID
$OK_JSON"
new_draft "$DRAFT-vfresh" "$WORK/vfresh"
V_FRESH_TREE=$(draft_tree "$DRAFT-vfresh" 2)
crux draft-delete "$DRAFT-vfresh" >/dev/null
sql -c "UPDATE pack_trees SET created_at = '2000-01-01T00:00:00Z' WHERE digest IN ('$V_GC_TREE', '$V_TREE')"
V_PINS=$(v_pins)

stop_controller
start_controller
[ "$(conversion_report "$BOOT")" = none ] || fail "[V] boot $BOOT converted again: $(conversion_report "$BOOT")"
[ "$(v_pins)" = "$V_PINS" ] || fail "[V] a restart moved the pins to $(v_pins)"
[ "$(collected_report "$BOOT")" = 1 ] || fail "[V] boot $BOOT collected '$(collected_report "$BOOT")' trees, not the one unpinned old tree"
[ "$(sql -F ' ' -c "SELECT (SELECT count(*) FROM pack_trees WHERE digest = '$V_GC_TREE'), (SELECT count(*) FROM pack_tree_files WHERE digest = '$V_GC_TREE')")" = "0 0" ] ||
    fail "[V] the unpinned old tree is still stored"
for kept in "$V_TREE" "$V_FRESH_TREE"; do
    [ -n "$(tree_tarball "$kept")" ] && [ -n "$(tree_paths "$kept")" ] || fail "[V] collection removed $kept"
done
pass "[V] a restart collected the unpinned old tree and kept the pinned one and the fresh one; the pins held"

expect_released V-restart e2e-vpub
crux draft-publish "$DRAFT-vpin" --playbook e2e-vpub --json >"$WORK/publish-V-same.json"
[ "$(jq -r .rev "$WORK/publish-V-same.json")" = "$V_TREE" ] && [ "$(v_pins)" = "$V_PINS" ] ||
    fail "[V] the identical republish moved the pins to $(v_pins)"
expect_released V-same e2e-vpub
pass "[V] the binding is not stale after a restart and an identical republish"

echo '# version 3' >>"$WORK/vpin/check.sh"
crux draft-push "$DRAFT-vpin" "$WORK/vpin" --base-version 2 --json >"$WORK/push-V3.json"
crux draft-publish "$DRAFT-vpin" --playbook e2e-vpub --json >"$WORK/publish-V3.json"
V_TREE2=$(playbook_tree e2e-vpub)
[ "$V_TREE2" != "$V_TREE" ] && [ "$(jq -r .rev "$WORK/publish-V3.json")" = "$V_TREE2" ] || fail "[V] the republish registered '$V_TREE2'"
launch V-stale launch e2e-vpub
settle V-stale "$KEY"
expect_parked V-stale "against pack revision $V_TREE, which the pin bump to $V_TREE2 moved"
api GET /api/playbook-drafts/e2e-vfork
[ "$(jq -c '.origin | [.digest, .current_digest, .moved]' "$WORK/api.json")" = "[\"$V_TREE\",\"$V_TREE2\",true]" ] ||
    fail "[V] the fork's origin reads $(jq -c .origin "$WORK/api.json")"
pass "[V] a republish of new content stales the binding and moves the fork"

log "[V] republishing between a launch and its dispatch"
api DELETE "/api/secrets/$V_SECRET/bindings/$V_BINDING"
[ "$HTTP" = 200 ] || [ "$HTTP" = 204 ] || fail "[V] unbinding: $HTTP $(cat "$WORK/api.json")"
bind_vpin "$V_TREE2"
expect_http V 201 '"scope_id":"e2e-vpub"'
V_BINDING=$(jq -r .id "$WORK/api.json")
hold_dispatch
launch V-frozen launch e2e-vpub
V_FROZEN="$KEY"
held "$V_FROZEN"
[ "$(launch_tree "$V_FROZEN")" = "$V_TREE2" ] || fail "[V] the queued launch froze $(launch_tree "$V_FROZEN")"
echo '# version 4' >>"$WORK/vpin/check.sh"
crux draft-push "$DRAFT-vpin" "$WORK/vpin" --base-version 3 --json >"$WORK/push-V4.json"
crux draft-publish "$DRAFT-vpin" --playbook e2e-vpub --json >"$WORK/publish-V4.json"
V_TREE3=$(playbook_tree e2e-vpub)
[ "$V_TREE3" != "$V_TREE2" ] || fail "[V] the second republish kept $V_TREE2"
held "$V_FROZEN"
release_dispatch
settle V-frozen "$V_FROZEN"
expect_parked V-frozen "this launch resolved 1 binding(s)"
launch V-repinned launch e2e-vpub
settle V-repinned "$KEY"
expect_parked V-repinned "against pack revision $V_TREE2, which the pin bump to $V_TREE3 moved"
pass "[V] a launch is checked against the tree it froze, not the registry's newer one"

stop_controller
log "[V] storing the collected tree's legacy bytes again"
seed_pack_row kind_e2e_vgc2 "$WORK/vgc.tar.gz"
log "[V] booting without a dev identity, so callers assert their login"
VHREPO="$WORK/vhrepo"
small_pack "$VHREPO/pack" "echo vhook-$ID
$OK_JSON"
cp -R "$VHREPO/pack" "$VHREPO/pack2"
echo '# the same commit, another tree' >>"$VHREPO/pack2/check.sh"
git -C "$VHREPO" init -q
git_commit "$VHREPO" hook
VH_SOURCE="\"repo\":\"file://$VHREPO\",\"path\":\"pack\""
CONTROLLER_ENV=(CONTROLLER_DEV_IDENTITY=)
start_controller
read -r converted unconvertible <<<"$(conversion_report "$BOOT")"
[ "$converted $unconvertible" = "1 0" ] || fail "[V] boot $BOOT reported '$converted $unconvertible' storing the collected bytes again"
grep -q 'pack tree conversion failed' "$WORK/controller-$BOOT.log" && fail "[V] conversion failed on bytes whose tree was collected"
[ "$(launch_tree kind_e2e_vgc2)" = "$V_GC_TREE" ] && [ "$(tree_paths "$V_GC_TREE")" = crucible.toml ] ||
    fail "[V] the collected tree's bytes converted to '$(launch_tree kind_e2e_vgc2)' holding '$(tree_paths "$V_GC_TREE")'"
pass "[V] legacy bytes whose aliased tree was collected store that tree again"

as e2e POST /api/playbooks "{\"id\":\"e2e-vhook\",\"description\":\"kind e2e\",$VH_SOURCE}"
expect_http V 201 '"id":"e2e-vhook"'
VH_TREE=$(playbook_tree e2e-vhook)
VH_REV=$(sql -c "SELECT rev FROM playbooks WHERE id = 'e2e-vhook'")
as e2e PUT /api/playbooks/e2e-vhook/shares/user:outsider '{"role":"launcher"}'
[ "$HTTP" = 200 ] || [ "$HTTP" = 201 ] || [ "$HTTP" = 204 ] || fail "[V] sharing e2e-vhook: $HTTP $(cat "$WORK/api.json")"
VH_BODY='{"playbook":"e2e-vhook","verifier":"path_token","dedupe":"string(body.n)","max_launches_per_hour":1,"max_cost":1,"max_time":"5m"}'
as outsider POST /api/webhooks "$VH_BODY"
expect_http V 201 '"playbook":"e2e-vhook"'
VH_HOOK=$(jq -r .webhook.id "$WORK/api.json")
as e2e PUT /api/playbooks/e2e-vhook/shares/user:outsider '{"role":"viewer"}'
[ "$HTTP" = 200 ] || [ "$HTTP" = 201 ] || [ "$HTTP" = 204 ] || fail "[V] narrowing the share: $HTTP $(cat "$WORK/api.json")"
as outsider PUT "/api/webhooks/$VH_HOOK" "$VH_BODY"
[ "$HTTP" = 200 ] || fail "[V] an unchanged webhook edit by a viewer: $HTTP $(cat "$WORK/api.json")"
as e2e POST /api/playbook-drafts "{\"id\":\"e2e-vhfork\",\"description\":\"kind e2e\",\"template\":\"e2e-vhook\",\"template_rev\":\"$VH_REV\",\"template_digest\":\"$VH_TREE\"}"
expect_http V 201 '"version":1'
as e2e POST /api/playbooks "{\"id\":\"e2e-vhook\",\"description\":\"kind e2e\",\"repo\":\"file://$VHREPO\",\"path\":\"pack2\"}"
expect_http V 201 '"id":"e2e-vhook"'
VH_TREE2=$(playbook_tree e2e-vhook)
[ "$(sql -c "SELECT rev FROM playbooks WHERE id = 'e2e-vhook'")" = "$VH_REV" ] && [ "$VH_TREE2" != "$VH_TREE" ] ||
    fail "[V] re-registering at the same commit holds rev $(sql -c "SELECT rev FROM playbooks WHERE id = 'e2e-vhook'") and tree $VH_TREE2"
as outsider PUT "/api/webhooks/$VH_HOOK" "$VH_BODY"
[ "$HTTP" = 403 ] || [ "$HTTP" = 404 ] || fail "[V] a viewer re-adopted a new tree at the same rev: $HTTP $(cat "$WORK/api.json")"
[ "$(sql -c "SELECT adopted_tree_digest FROM playbook_standing_launches WHERE id = '$VH_HOOK'")" = "$VH_TREE" ] ||
    fail "[V] the refused edit changed the adopted tree"
as e2e PUT "/api/webhooks/$VH_HOOK" "$VH_BODY"
[ "$HTTP" = 200 ] && [ "$(jq -r .adopted_tree_digest "$WORK/api.json")" = "$VH_TREE2" ] ||
    fail "[V] the owner's edit: $HTTP $(cat "$WORK/api.json")"
sql -c "SELECT reason FROM events WHERE key LIKE '%$VH_HOOK'" | grep -qF "deliveries as revision $VH_REV (tree $VH_TREE2)" ||
    fail "[V] the webhook edit's audit names: $(sql -c "SELECT reason FROM events WHERE key LIKE '%$VH_HOOK'")"
pass "[V] a same-rev tree change makes a webhook edit a new firing, decided against the playbook again"
as e2e GET /api/playbook-drafts/e2e-vhfork
[ "$(jq -c '.origin | [.rev == .current_rev, .digest, .current_digest, .moved]' "$WORK/api.json")" = "[true,\"$VH_TREE\",\"$VH_TREE2\",true]" ] ||
    fail "[V] the fork of the same-rev repoint reads $(jq -c .origin "$WORK/api.json")"
pass "[V] a fork moves when its origin's tree changes under the same rev"

for who in outsider e2e; do
    for pin in old unknown; do
        digest="$V_OLD"
        [ "$pin" = unknown ] && digest="$V_UNKNOWN"
        as "$who" POST /api/playbook-drafts "{\"id\":\"e2e-vsup\",\"description\":\"kind e2e\",\"template\":\"e2e-vpub\",\"template_digest\":\"$digest\"}"
        cp "$WORK/api.json" "$WORK/template-$who-$pin.json"
        echo "$HTTP" >"$WORK/template-$who-$pin.code"
    done
done
cmp -s "$WORK/template-outsider-old.json" "$WORK/template-outsider-unknown.json" &&
    [ "$(cat "$WORK/template-outsider-old.code")" = "$(cat "$WORK/template-outsider-unknown.code")" ] ||
    fail "[V] a non-reader told the old digest $(cat "$WORK/template-outsider-old.code") $(cat "$WORK/template-outsider-old.json") apart from an unknown one $(cat "$WORK/template-outsider-unknown.code") $(cat "$WORK/template-outsider-unknown.json")"
{ grep -qF "superseded by $V_TREE" "$WORK/template-e2e-old.json" && ! grep -q superseded "$WORK/template-e2e-unknown.json"; } ||
    fail "[V] the reader got $(cat "$WORK/template-e2e-old.json") and $(cat "$WORK/template-e2e-unknown.json")"
grep -q "$V_TREE" "$WORK/template-outsider-old.json" && fail "[V] the non-reader's answer names $V_TREE"
pass "[V] a non-reader's answer for the pre-tree digest is byte-identical to the unknown digest's; a reader's names the replacement"

# ---- scenario G ----------------------------------------------------------------------------
stop_controller
log "[G] migrating a legacy state dir"
OLD="$WORK/oldstate/packs/kind_e2e_legacy"
small_pack "$OLD" "echo migrated-$ID
$OK_JSON"
printf 'frozen guidance\n<!-- steer @1790812800 by control -->\nfirst\n<!-- steer @1790812900 by control -->\nsecond\n' >"$OLD/STEER.md"
"$BIN/crucible-controller" db migrate-state --state-dir "$WORK/oldstate" --db "$DB" >"$WORK/migrate-G.log" 2>&1 ||
    fail "[G] migrate-state failed: $(cat "$WORK/migrate-G.log")"
if ! grep -Eq '^packs +1 +0 +0$' "$WORK/migrate-G.log" || ! grep -Eq '^steering +2 +0 +0$' "$WORK/migrate-G.log"; then
    fail "[G] migrate-state reported: $(cat "$WORK/migrate-G.log")"
fi
G_TREE=$(launch_tree kind_e2e_legacy)
is_tree "$G_TREE" || fail "[G] the migrated pack holds tree '$G_TREE'"
[ "$(tree_file "$G_TREE" STEER.md)" = "frozen guidance" ] || fail "[G] the stored STEER.md is: $(tree_file "$G_TREE" STEER.md)"
[ "$(sql -c "SELECT string_agg(body_md, ',' ORDER BY seq) FROM pack_steering WHERE issue_slug = 'kind_e2e_legacy'")" = "first,second" ] ||
    fail "[G] the steering rows are: $(sql -c "SELECT seq, body_md FROM pack_steering WHERE issue_slug = 'kind_e2e_legacy'")"
pass "[G] the legacy pack dir became a tree with its steering split into rows"
"$BIN/crucible-controller" db migrate-state --state-dir "$WORK/oldstate" --db "$DB" >"$WORK/migrate-G2.log" 2>&1 ||
    fail "[G] the second migrate-state failed: $(cat "$WORK/migrate-G2.log")"
if ! grep -Eq '^packs +0 +1 +0$' "$WORK/migrate-G2.log" || ! grep -Eq '^steering +0 +2 +0$' "$WORK/migrate-G2.log"; then
    fail "[G] the second migrate-state reported: $(cat "$WORK/migrate-G2.log")"
fi
[ "$(launch_tree kind_e2e_legacy)" = "$G_TREE" ] &&
    [ "$(sql -c "SELECT count(*) FROM pack_steering WHERE issue_slug = 'kind_e2e_legacy'")" = 2 ] ||
    fail "[G] the second migrate-state changed the migrated pack"
pass "[G] a second migrate-state leaves the migrated pack as it was"

# ---- scenario M ----------------------------------------------------------------------------
CONTROLLER_ENV=()
start_controller "$WORK/profile-mismatch.toml"
curl -sf "$CONTROLLER_URL/api/config" >"$WORK/config-M.json"
[ "$(jq -r --arg ref "$MISMATCH_IMAGE" '.contract.images[] | select(.reference == $ref) | "\(.engine_version) \(.match)"' "$WORK/config-M.json")" = "0.0.0 false" ] ||
    fail "[M] /api/config does not report the mismatch: $(jq -c .contract "$WORK/config-M.json")"
watch_start M
launch M launch "$DRAFT-pub"
settle M "$KEY"
watch_stop
expect_parked M "contract rejection"
[ "$(pods_seen M "$KEY")" = 0 ] || fail "[M] a pod was created for a rejected launch"
pass "[M] no pod was created"

# ---- scenario Z ----------------------------------------------------------------------------
stop_controller
log "[Z] rebuilding the ledger"
PACK_STORE="SELECT 'row', issue_slug, tree_digest FROM pack_tarballs WHERE tree_digest IS NOT NULL
UNION ALL SELECT 'files', digest, count(*)::TEXT FROM pack_tree_files
    WHERE digest IN (SELECT tree_digest FROM pack_tarballs) GROUP BY digest
UNION ALL SELECT 'alias', old_digest, coalesce(tree_digest, unconvertible_reason) FROM pack_digest_aliases
ORDER BY 1, 2"
sql -c "$PACK_STORE" >"$WORK/pack-store-before.txt"
[ "$(grep -c '^row|' "$WORK/pack-store-before.txt")" -gt 10 ] || fail "[Z] too few pack rows to compare"
DATABASE_URL="$DB" "$BIN/crucible-controller" db rebuild --state-dir "$WORK/state" >"$WORK/rebuild-Z.log" 2>&1 ||
    fail "[Z] db rebuild failed: $(tail -n 20 "$WORK/rebuild-Z.log")"
sql -c "$PACK_STORE" >"$WORK/pack-store-after.txt"
diff "$WORK/pack-store-before.txt" "$WORK/pack-store-after.txt" >"$WORK/pack-store.diff" ||
    fail "[Z] the rebuild changed the pack store:
$(cat "$WORK/pack-store.diff")"
pass "[Z] the rebuilt ledger holds every pack row's tree, its files, and every alias"

echo "kind e2e passed"
