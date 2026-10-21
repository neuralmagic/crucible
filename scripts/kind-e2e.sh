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
#   U  a save stores only the file contents not already stored: an identical pack in another draft
#      adds no blob, a save with one file changed adds one, and drafts share the blobs they have
#      in common; collecting a deleted draft's tree deletes only the blob no other tree holds
#   F  a failing task parks its launch with the task's error
#   H  with only the rendered sandbox Role (secrets create, no get or patch), a controller-
#      dispatched run pod and a rendered turn pod each boot the gateway (a stand-in that only
#      generates certs) far enough to create their client-TLS Secret, named for the pod and the
#      boot and owned by the pod, which collects it
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
#   N  with a peer controller standing by on the Lease and the discovery tick at its 300 s default,
#      a launch on the peer's API, a one-shot, an every-minute schedule, and a signed webhook
#      delivery to either replica each create their pod within seconds of being due; with the
#      leader killed, the peer takes the lease and fires a one-shot on time, once
#   M  a loop image labelled with another contract version parks the launch without a pod
#   Z  db rebuild carries the trees and aliases the pack rows reference
#   Y  examples/schema-repair through the controller: the draft's compiled graph and the run's
#      admitted plan parse with schema-typed fields and files, the agent whose first turn breaks
#      its file's schema passes after one repair turn, the mapped instance that never fixes its
#      file fails with a note that does not repeat the refused value, and the run's spend and the
#      task results count the repair turns
# The scenarios run in three shards at once, each on its own kind cluster, database, and controller
# ports, sharing one registry and loop image: store (A P U F C B D L T I W E X Q V G M Z), pods
# (H R S K Y), and lease (O N), in scripts/kind-e2e/<shard>.sh. Arguments name the shards to run.
# Needs docker, kind, kubectl, jq, curl, git, shasum, openssl. Uses $DATABASE_URL and $PG_CONTAINER when
# set (the CI postgres action), else starts its own Postgres. KEEP=1 leaves everything up;
# ARTIFACT_DIR collects logs on failure.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
FIX="$ROOT/e2e/kind"
ID="${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-$$}"
REG="crucible-e2e-reg-$ID"
# Every listen port sits below the kernel's ephemeral range: a port inside it is handed to outbound
# connections as their source port, and one such socket between two boots blocks the rebind. Shard i
# listens on these plus i.
REGPORT="${REGPORT:-$((20000 + RANDOM % 2000))}"
PORT="${PORT:-$((22000 + RANDOM % 2000))}"
HOOKPORT="${HOOKPORT:-$((24000 + RANDOM % 2000))}"
PEERPORT="${PEERPORT:-$((26000 + RANDOM % 2000))}"
PEERHOOKPORT="${PEERHOOKPORT:-$((28000 + RANDOM % 2000))}"
NS=crucible-e2e
DRAFT=e2e
SKOPEO=quay.io/skopeo/stable:v1.20
SHARDS=("$@")
[ ${#SHARDS[@]} -gt 0 ] || SHARDS=(store pods lease)
mkdir -p "$ROOT/target"
TOP=$(mktemp -d "$ROOT/target/kind-e2e.XXXXXX")
WORK="$TOP"
ARTIFACTS="${ARTIFACT_DIR:-$TOP/artifacts}"
OWN_PG=""
PG=""
BUILD_PID=""
CREATE_PIDS=()
SHARD_PIDS=()
CONTROLLER_PID=""
PEER_PID=""
CONTROLLER_ENV=()
BG_PIDS=()
API_PIDS=()

for tool in docker kind kubectl jq curl git shasum openssl; do
    command -v "$tool" >/dev/null || { echo "missing $tool" >&2; exit 1; }
done
for shard in "${SHARDS[@]}"; do
    [ -f "$ROOT/scripts/kind-e2e/$shard.sh" ] || { echo "no shard $shard in scripts/kind-e2e" >&2; exit 1; }
done

log() { echo "==> $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

if [ -r /proc/sys/net/ipv4/ip_local_port_range ]; then
    read -r EPHEMERAL_LOW EPHEMERAL_HIGH </proc/sys/net/ipv4/ip_local_port_range
else
    EPHEMERAL_LOW=$(sysctl -n net.inet.ip.portrange.first)
    EPHEMERAL_HIGH=$(sysctl -n net.inet.ip.portrange.last)
fi
for i in "${!SHARDS[@]}"; do
    for p in "$REGPORT" $((PORT + i)) $((HOOKPORT + i)) $((PEERPORT + i)) $((PEERHOOKPORT + i)); do
        if [ "$p" -ge "$EPHEMERAL_LOW" ] && [ "$p" -le "$EPHEMERAL_HIGH" ]; then
            fail "port $p is inside the ephemeral range $EPHEMERAL_LOW-$EPHEMERAL_HIGH, where any outbound connection can take it"
        fi
    done
done

cluster_name() { echo "crucible-e2e-$ID-$1"; }

collect() {
    mkdir -p "$ARTIFACT_DIR"
    cp "$WORK"/*.log "$WORK"/*.json "$ARTIFACT_DIR"/ 2>/dev/null || true
    kubectl get events -A --sort-by=.lastTimestamp >"$ARTIFACT_DIR/events.txt" 2>&1 || true
    kubectl -n "$NS" get all,cm,pvc -o yaml >"$ARTIFACT_DIR/objects.yaml" 2>&1 || true
    kind export logs --name "$CLUSTER" "$ARTIFACT_DIR/kind" >/dev/null 2>&1 || true
    sql -c "SELECT key, status, parked_reason FROM issues" </dev/null >"$ARTIFACT_DIR/issues.txt" 2>&1 || true
    echo "artifacts: $ARTIFACT_DIR" >&2
}

# halt <pid>: SIGTERM, reaped; a process still running 30s later is SIGKILLed and halt returns 1.
halt() {
    kill "$1" 2>/dev/null || return 0
    for _ in $(seq 1 120); do
        kill -0 "$1" 2>/dev/null || break
        sleep 0.25
    done
    local rc=0
    if kill -0 "$1" 2>/dev/null; then
        kill -9 "$1" 2>/dev/null || true
        rc=1
    fi
    wait "$1" 2>/dev/null || true
    return "$rc"
}

cleanup() {
    local rc=$? p shard
    for p in $BUILD_PID ${SHARD_PIDS[@]+"${SHARD_PIDS[@]}"}; do kill "$p" 2>/dev/null || true; done
    for p in ${CREATE_PIDS[@]+"${CREATE_PIDS[@]}"}; do wait "$p" 2>/dev/null || true; done
    if [ "$rc" -ne 0 ]; then
        mkdir -p "$ARTIFACTS"
        docker logs "$REG" >"$ARTIFACTS/registry.log" 2>&1 || true
        echo "artifacts: $ARTIFACTS" >&2
    fi
    if [ "${KEEP:-0}" = 1 ]; then
        echo "KEEP=1: registry $REG, work $TOP"
        return
    fi
    for shard in "${SHARDS[@]}"; do kind delete cluster --name "$(cluster_name "$shard")" >/dev/null 2>&1 || true; done
    docker rm -f "$REG" >/dev/null 2>&1 || true
    [ -n "$OWN_PG" ] && docker rm -f "$OWN_PG" >/dev/null 2>&1
    [ "$rc" -eq 0 ] && rm -rf "$TOP"
    return 0
}
trap cleanup EXIT

# shard_cleanup: halt the shard's controllers and watches, and collect its logs on failure.
shard_cleanup() {
    local rc=$? p
    for p in "$CONTROLLER_PID" "$PEER_PID"; do
        [ -n "$p" ] || continue
        halt "$p" || true
    done
    for p in ${BG_PIDS[@]+"${BG_PIDS[@]}"} ${API_PIDS[@]+"${API_PIDS[@]}"}; do kill "$p" 2>/dev/null || true; done
    [ "$rc" -ne 0 ] && collect
    [ "${KEEP:-0}" = 1 ] && echo "KEEP=1: cluster $CLUSTER, work $WORK, controller port $PORT"
    return 0
}

sql() { docker exec -i "$PG" psql -qAt -v ON_ERROR_STOP=1 -U postgres -d "$DBNAME" "$@"; }

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

# ---- controller ----------------------------------------------------------------------------
export CONTROLLER_CONFIG=/dev/null
unset CONTROLLER_API_TOKEN

BOOT=0
# DISCOVERY_SECS is the discovery cadence every boot runs with; empty leaves the controller default.
DISCOVERY_SECS=5
# run_controller <api port> <hooks port> <log> <profile>: start CONTROLLER_BIN in the background.
# CONTROLLER_ENV holds extra VAR=value settings for this boot.
run_controller() {
    env -u CONTROLLER_PROXY_TOKEN -u CONTROLLER_OIDC_ISSUER -u VAULT_ADDR \
        -u POD_NAME -u POD_NAMESPACE -u CONTROLLER_DISCOVERY_CADENCE_SECS \
        DATABASE_URL="$DB" \
        CONTROLLER_API_ADDR="127.0.0.1:$1" CONTROLLER_PUBLIC_URL="http://127.0.0.1:$1" \
        CONTROLLER_HOOKS_ADDR="127.0.0.1:$2" \
        ${DISCOVERY_SECS:+CONTROLLER_DISCOVERY_CADENCE_SECS=$DISCOVERY_SECS} \
        CONTROLLER_DEV_IDENTITY=e2e CONTROLLER_ADMINS=e2e CONTROLLER_AUTH_MODE=proxy \
        CONTROLLER_SESSION_SECURE=false \
        CONTROLLER_PLAYBOOK_EXECUTOR=pod CONTROLLER_SCOPE_EXECUTOR=disabled \
        CONTROLLER_DEPLOY_PROFILE="$4" CONTROLLER_POD_NAMESPACE="$NS" \
        CONTROLLER_TURN_SERVICE_ACCOUNT=crucible-loop \
        CONTROLLER_STATE_DIR="$WORK/state" CONTROLLER_SCRATCH_DIR="$WORK/scratch" \
        CRUCIBLE_BIN="$BIN/crucible" FORGE_INSECURE_REGISTRIES="localhost:$REGPORT" \
        RUST_LOG="${RUST_LOG:-info}" NO_COLOR=1 \
        ${CONTROLLER_ENV[@]+"${CONTROLLER_ENV[@]}"} \
        "$CONTROLLER_BIN" autopilot >"$3" 2>&1 &
}

# ports_free <what> <port>...: wait up to 30s for nothing to accept on each loopback port.
ports_free() {
    local what="$1" p
    shift
    for p in "$@"; do
        for _ in $(seq 1 30); do
            (exec 3<>"/dev/tcp/127.0.0.1/$p") 2>/dev/null || continue 2
            sleep 1
        done
        fail "$what: 127.0.0.1:$p still accepts connections before the boot: $(ss -Hltnp "sport = :$p" 2>/dev/null || true)"
    done
}

# await_health <what> <pid> <url> <log>: wait for <url>/healthz, failing early when <pid> exits.
await_health() {
    local what="$1" pid="$2" url="$3" log="$4"
    for _ in $(seq 1 90); do
        curl -sf "$url/healthz" >/dev/null 2>&1 && return 0
        kill -0 "$pid" 2>/dev/null || fail "$what exited before answering $url; its log ends:
$(tail -n 20 "$log")"
        sleep 1
    done
    fail "timed out after 90s waiting for $what health"
}

# start_controller [profile]: boot CONTROLLER_BIN on PORT and wait for it to answer and lead.
start_controller() {
    BOOT=$((BOOT + 1))
    log "starting controller (boot $BOOT) on $CONTROLLER_URL ${CONTROLLER_ENV[*]+${CONTROLLER_ENV[*]}}"
    ports_free "controller (boot $BOOT)" "$PORT" "$HOOKPORT"
    run_controller "$PORT" "$HOOKPORT" "$WORK/controller-$BOOT.log" "${1:-$WORK/profile.toml}"
    CONTROLLER_PID=$!
    await_health "controller (boot $BOOT)" "$CONTROLLER_PID" "$CONTROLLER_URL" "$WORK/controller-$BOOT.log"
    wait_for 180 "startup pack conversion" grep -q 'issues re-enqueued at startup' "$WORK/controller-$BOOT.log"
}

stop_controller() {
    halt "$CONTROLLER_PID" || fail "controller (boot $BOOT) was still running 30s after SIGTERM"
    CONTROLLER_PID=""
}

# start_peer: boot a second controller on PEERPORT, sharing the database and cluster, and wait for
# it to answer. Its log is $WORK/controller-peer.log.
start_peer() {
    log "starting the peer controller on $PEER_URL ${CONTROLLER_ENV[*]+${CONTROLLER_ENV[*]}}"
    ports_free "peer controller" "$PEERPORT" "$PEERHOOKPORT"
    run_controller "$PEERPORT" "$PEERHOOKPORT" "$WORK/controller-peer.log" "$WORK/profile.toml"
    PEER_PID=$!
    await_health "peer controller" "$PEER_PID" "$PEER_URL" "$WORK/controller-peer.log"
}

stop_peer() {
    halt "$PEER_PID" || fail "the peer controller was still running 30s after SIGTERM"
    PEER_PID=""
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
    docker exec -i "$PG" psql -qAt -v ON_ERROR_STOP=1 -U postgres -d "$DBNAME" \
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
    [ "$(sql -c "SELECT count(*) FROM pg_stat_activity WHERE datname = '$DBNAME' AND application_name = 'psql' AND state = 'idle in transaction'")" = 1 ]
}

# lock_waiters <n>: exactly n sessions are waiting on a lock.
lock_waiters() {
    [ "$(sql -c "SELECT count(*) FROM pg_stat_activity WHERE datname = '$DBNAME' AND wait_event_type = 'Lock'")" = "$1" ]
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

# collected_report <boot>: how many unpinned trees and blobs the boot collected, "none" if silent.
collected_report() {
    local line
    line=$(grep 'unpinned pack trees collected' "$WORK/controller-$1.log" || true)
    [ -z "$line" ] && { echo none; return; }
    echo "$(sed -E 's/.*count=([0-9]+).*/\1/' <<<"$line") $(sed -E 's/.*blobs=([0-9]+).*/\1/' <<<"$line")"
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
tree_file() { sql -c "SELECT convert_from(b.content, 'UTF8') FROM pack_tree_files f JOIN pack_blobs b USING (sha256) WHERE f.digest = '$1' AND f.path = '$2'"; }
# tamper_file <tree> <path>: overwrite the stored bytes of one file of a tree.
tamper_file() { sql -c "UPDATE pack_blobs SET content = 'tampered' WHERE sha256 = (SELECT sha256 FROM pack_tree_files WHERE digest = '$1' AND path = '$2')"; }
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

# run_shard <name> <index>: the scenarios in scripts/kind-e2e/<name>.sh against the shard's own
# cluster, database, and controller ports.
run_shard() {
    WORK="$TOP/$1"
    ARTIFACT_DIR="$ARTIFACTS/$1"
    CLUSTER=$(cluster_name "$1")
    DBNAME="kind_e2e_$1"
    DB="postgres://postgres:ci@127.0.0.1:$PG_PORT/$DBNAME"
    PORT=$((PORT + $2)) HOOKPORT=$((HOOKPORT + $2)) PEERPORT=$((PEERPORT + $2)) PEERHOOKPORT=$((PEERHOOKPORT + $2))
    export KUBECONFIG="$WORK/kubeconfig" CONTROLLER_URL="http://127.0.0.1:$PORT"
    HOOKS_URL="http://127.0.0.1:$HOOKPORT"
    PEER_URL="http://127.0.0.1:$PEERPORT"
    PEER_HOOKS_URL="http://127.0.0.1:$PEERHOOKPORT"
    trap shard_cleanup EXIT

    cp "$TOP"/profile*.toml "$WORK/"
    for node in $(kind get nodes --name "$CLUSTER"); do
        docker exec "$node" mkdir -p "/etc/containerd/certs.d/localhost:$REGPORT"
        printf '[host."http://%s:5000"]\n' "$REG" |
            docker exec -i "$node" tee "/etc/containerd/certs.d/localhost:$REGPORT/hosts.toml" >/dev/null
    done
    kubectl create namespace "$NS" >/dev/null
    kubectl -n "$NS" create serviceaccount crucible-loop >/dev/null
    # The loop pod's kubeconfig: the API server through the projected token and CA it mounts.
    cat >"$WORK/pod-kubeconfig" <<KUBECONFIG
apiVersion: v1
kind: Config
clusters:
  - name: in-cluster
    cluster:
      server: https://kubernetes.default.svc
      certificate-authority: /var/run/kube/ca.crt
users:
  - name: loop
    user:
      tokenFile: /var/run/kube/token
contexts:
  - name: in-cluster
    context:
      cluster: in-cluster
      user: loop
      namespace: $NS
current-context: in-cluster
KUBECONFIG
    kubectl -n "$NS" create configmap crucible-e2e-kubeconfig --from-file=kubeconfig="$WORK/pod-kubeconfig" >/dev/null
    docker exec "$PG" psql -qAt -U postgres -d crucible -c "DROP DATABASE IF EXISTS $DBNAME" -c "CREATE DATABASE $DBNAME"

    source "$ROOT/scripts/kind-e2e/$1.sh"
    echo "kind e2e $1 passed"
}

# ---- clusters, build, and postgres ---------------------------------------------------------
log "creating kind clusters for ${SHARDS[*]}"
for shard in "${SHARDS[@]}"; do
    mkdir -p "$TOP/$shard"
    kind create cluster -q --name "$(cluster_name "$shard")" --config "$FIX/cluster.yaml" --wait 120s \
        --kubeconfig "$TOP/$shard/kubeconfig" >"$TOP/$shard/kind-create.log" 2>&1 &
    CREATE_PIDS+=($!)
done

if [ -n "${PG_CONTAINER:-}" ]; then
    PG="$PG_CONTAINER"
else
    PG="crucible-e2e-pg-$ID"
    OWN_PG="$PG"
    docker run -d -q --name "$PG" -e POSTGRES_PASSWORD=ci -e POSTGRES_DB=crucible \
        --tmpfs /var/lib/postgresql/data:rw -p 127.0.0.1::5432 postgres:16 -c max_connections=400 >/dev/null
fi

NODE_ARCH=$(docker info -f '{{.Architecture}}')
case "$NODE_ARCH" in x86_64 | amd64) NODE_ARCH=amd64 ;; aarch64 | arm64) NODE_ARCH=arm64 ;; esac

if [ "${SKIP_BUILD:-0}" != 1 ]; then
    log "building the controller, crux, and crucible for the host, beside both with the autoresearch lane"
    (cd "$ROOT" && SQLX_OFFLINE=true cargo build --locked -q -p crucible -p crucible-controller -p crux --bins) &
    BUILD_PID=$!
    (cd "$ROOT" && SQLX_OFFLINE=true cargo build --locked -q -p crucible-controller --features autoresearch \
        --bin crucible-controller --target-dir "$ROOT/target/autoresearch")
    (cd "$ROOT" && SQLX_OFFLINE=true cargo build --locked -q -p crucible --features autoresearch \
        --bin crucible --target-dir "$ROOT/target/autoresearch")
fi
BIN="$ROOT/target/debug"
AUTORESEARCH_CONTROLLER="$ROOT/target/autoresearch/debug/crucible-controller"
AUTORESEARCH_ENGINE="$ROOT/target/autoresearch/debug/crucible"
CONTROLLER_BIN="$BIN/crucible-controller"

mkdir -p "$TOP/image"
HOST_ARCH=$(uname -m)
case "$HOST_ARCH" in x86_64) HOST_ARCH=amd64 ;; aarch64) HOST_ARCH=arm64 ;; esac
if [ -n "${CRUCIBLE_LINUX_BIN:-}" ]; then
    cp "$CRUCIBLE_LINUX_BIN" "$TOP/image/crucible"
    LOOP_BASE="${LOOP_BASE:-ubuntu:24.04}"
elif [ "$(uname -s)" = Linux ] && [ "$HOST_ARCH" = "$NODE_ARCH" ]; then
    strip -o "$TOP/image/crucible" "$AUTORESEARCH_ENGINE"
    LOOP_BASE="${LOOP_BASE:-ubuntu:24.04}"
else
    log "building crucible for linux/$NODE_ARCH in rust:1-bookworm"
    docker run --rm --platform "linux/$NODE_ARCH" \
        -v "$ROOT":/src -w /src \
        -v kind-e2e-cargo:/usr/local/cargo/registry \
        -v "kind-e2e-target-$NODE_ARCH":/ctarget \
        -v "$TOP/image":/out \
        -e CARGO_TARGET_DIR=/ctarget -e SQLX_OFFLINE=true -e CARGO_PROFILE_DEV_DEBUG=0 \
        rust:1-bookworm sh -c 'cargo build --locked -q -p crucible --features autoresearch --bin crucible && cp /ctarget/debug/crucible /out/ && strip /out/crucible'
    LOOP_BASE="${LOOP_BASE:-debian:bookworm-slim}"
fi
CONTRACT_VERSION=$("$AUTORESEARCH_ENGINE" --contract-version)
cp "$ROOT/tools/fake-agent.py" "$TOP/image/fake-agent.py"
cp "$FIX/openshell-gateway" "$FIX/openshell" "$TOP/image/"

for p in ${CREATE_PIDS[@]+"${CREATE_PIDS[@]}"}; do
    wait "$p" || fail "creating a kind cluster failed: $(cat "$TOP"/*/kind-create.log)"
done
CREATE_PIDS=()

log "starting registry $REG on localhost:$REGPORT"
docker run -d -q --name "$REG" --network kind -p "127.0.0.1:$REGPORT:5000" registry:2 >/dev/null

# ---- loop images ---------------------------------------------------------------------------
# push_loop_image <tag> <contract label>: build the loop image and push it to the registry.
push_loop_image() {
    docker build -q --load --platform "linux/$NODE_ARCH" \
        --build-arg "BASE=$LOOP_BASE" --build-arg "CONTRACT_VERSION=$CONTRACT_VERSION" \
        --build-arg "CONTRACT_LABEL=$2" \
        -f "$FIX/Containerfile.loop" -t "crucible-e2e-loop:$1" "$TOP/image" >/dev/null
    docker save -o "$TOP/loop.tar" "crucible-e2e-loop:$1"
    docker run --rm -q --network kind -v "$TOP":/w "$SKOPEO" copy -q --dest-tls-verify=false \
        "docker-archive:/w/loop.tar" "docker://$REG:5000/crucible-e2e-loop:$1"
    rm "$TOP/loop.tar"
    docker rmi "crucible-e2e-loop:$1" >/dev/null 2>&1 || true
}
LOOP_IMAGE="localhost:$REGPORT/crucible-e2e-loop:$ID"
MISMATCH_IMAGE="localhost:$REGPORT/crucible-e2e-loop:$ID-mismatch"
log "building loop images on $LOOP_BASE (contract $CONTRACT_VERSION)"
push_loop_image "$ID" "$CONTRACT_VERSION"
push_loop_image "$ID-mismatch" 0.0.0
sed "s|@LOOP_IMAGE@|$LOOP_IMAGE|g" "$FIX/profile.toml.in" >"$TOP/profile.toml"
sed "s|@LOOP_IMAGE@|$MISMATCH_IMAGE|g" "$FIX/profile.toml.in" >"$TOP/profile-mismatch.toml"
awk '{ print } /^\[cluster\]$/ { print "state_pvc = \"crucible-e2e-state\"" }' "$TOP/profile.toml" >"$TOP/profile-state.toml"

if [ -n "$BUILD_PID" ]; then
    wait "$BUILD_PID" || fail "the host build failed"
    BUILD_PID=""
fi
wait_for 60 postgres docker exec "$PG" pg_isready -h 127.0.0.1 -U postgres -d crucible
PG_PORT=$(docker port "$PG" 5432/tcp | head -n1 | cut -d: -f2)

# ---- shards --------------------------------------------------------------------------------
for i in "${!SHARDS[@]}"; do
    shard=${SHARDS[$i]}
    run_shard "$shard" "$i" > >(awk -v p="[$shard] " '{ print p $0; fflush() }') 2>&1 &
    SHARD_PIDS+=($!)
done
FAILED=()
for i in "${!SHARDS[@]}"; do
    wait "${SHARD_PIDS[$i]}" || FAILED+=("${SHARDS[$i]}")
done
SHARD_PIDS=()
[ ${#FAILED[@]} -eq 0 ] || fail "shard(s) ${FAILED[*]} failed"
echo "kind e2e passed"
