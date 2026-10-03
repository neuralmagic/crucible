#!/usr/bin/env bash
# End-to-end proof of pack delivery against a real cluster: the controller from this checkout
# runs on the host with the pod executor, dispatches into a throwaway kind cluster, and pulls
# its loop image from a plain-HTTP registry:2 on the kind network.
#   A  a command-only draft launch completes; its pack arrives as one gzipped ConfigMap key
#      that pack-stage extracts, though the raw tar is over the ConfigMap size limit
#   B  legacy pack rows are converted to stored trees at startup: a foreign-encoded tarball
#      converts to the same tree, a symlink pack is recorded unconvertible, and the draft
#      still launches
#   C  a pack over the delivery budget is refused at save
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

# ---- loop image ----------------------------------------------------------------------------
LOOP_TAG="crucible-e2e-loop:$ID"
LOOP_IMAGE="localhost:$REGPORT/crucible-e2e-loop:$ID"
log "building $LOOP_TAG on $LOOP_BASE (contract $CONTRACT_VERSION)"
docker build -q --load --platform "linux/$NODE_ARCH" \
    --build-arg "BASE=$LOOP_BASE" --build-arg "CONTRACT_VERSION=$CONTRACT_VERSION" \
    -f "$FIX/Containerfile.loop" -t "$LOOP_TAG" "$WORK/image" >/dev/null
docker save -o "$WORK/image/loop.tar" "$LOOP_TAG"
docker run --rm -q --network kind -v "$WORK/image":/w "$SKOPEO" copy -q --dest-tls-verify=false \
    "docker-archive:/w/loop.tar" "docker://$REG:5000/crucible-e2e-loop:$ID"
docker rmi "$LOOP_TAG" >/dev/null 2>&1 || true
sed "s|@LOOP_IMAGE@|$LOOP_IMAGE|g" "$FIX/profile.toml.in" >"$WORK/profile.toml"

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
        CONTROLLER_DEPLOY_PROFILE="$WORK/profile.toml" CONTROLLER_POD_NAMESPACE="$NS" \
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

# launch_and_check <label>: launch the draft's newest save and assert the run and its delivery.
launch_and_check() {
    local label="$1" key status pod cm
    kubectl -n "$NS" get pods -w --output-watch-events -o json >"$WORK/pods-$label.json" 2>/dev/null &
    BG_PIDS+=($!)
    kubectl -n "$NS" get configmaps -w --output-watch-events -o json >"$WORK/cms-$label.json" 2>/dev/null &
    BG_PIDS+=($!)

    key=$(crux draft-launch "$DRAFT" --max-cost 1 --max-time 5m --json | tee "$WORK/launch-$label.json" | jq -r .key)
    [ -n "$key" ] && [ "$key" != null ] || fail "[$label] draft-launch returned no key"
    log "[$label] launched $key"

    status=""
    for _ in $(seq 1 300); do
        crux playbook-run "$key" >"$WORK/run-$label.json" 2>/dev/null || true
        status=$(jq -r '.launch.status // empty' "$WORK/run-$label.json" 2>/dev/null || true)
        case "$status" in done | pr-open | parked) break ;; esac
        sleep 1
    done
    [ "$status" = "done" ] || fail "[$label] launch ended '$status': $(jq -c '.launch' "$WORK/run-$label.json" 2>/dev/null)"
    [ "$(jq -r '.runs[0].status' "$WORK/run-$label.json")" = finished ] ||
        fail "[$label] run did not finish: $(jq -c '.runs[0]' "$WORK/run-$label.json")"
    pass "[$label] launch done, run finished"

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
    local gz raw listing
    gz=$(wc -c <"$WORK/delivered-$label.tar.gz" | tr -d ' ')
    raw=$(gzip -dc "$WORK/delivered-$label.tar.gz" | wc -c | tr -d ' ')
    [ "$gz" -le 921600 ] || fail "[$label] delivered tarball is $gz bytes, over the budget"
    [ "$raw" -gt 1048576 ] || fail "[$label] raw tar is only $raw bytes; the fixture no longer exceeds the ConfigMap limit"
    listing=$(tar -tzf "$WORK/delivered-$label.tar.gz" | sort | tr '\n' ' ')
    [ "$listing" = "bulk/a.txt bulk/b.txt bulk/c.txt check.sh crucible.toml nested/deep/marker.txt workflow.star " ] ||
        fail "[$label] delivered tarball lists: $listing"
    pass "[$label] pack delivered as one gzipped key ($gz bytes gzipped, $raw raw)"

    for p in ${BG_PIDS[@]+"${BG_PIDS[@]}"}; do kill "$p" 2>/dev/null || true; done
    BG_PIDS=()
}

# ---- scenario A ----------------------------------------------------------------------------
start_controller
[ "$(crux whoami 2>/dev/null | head -n1)" != "" ] || fail "crux cannot reach the controller"

log "[A] creating draft $DRAFT and saving the delivery pack"
crux draft-create "$DRAFT" --description "kind e2e" >/dev/null
crux draft-push "$DRAFT" "$PACK" --base-version 1 --json >"$WORK/push-A.json"
VERSION=$(jq -r .version "$WORK/push-A.json")
[ "$VERSION" = 2 ] || fail "[A] draft-push saved version '$VERSION': $(cat "$WORK/push-A.json")"
launch_and_check A

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
[ "$converted" -ge 2 ] || fail "[B] boot $BOOT converted only $converted packs"
[ "$(sql -c "SELECT count(*) FROM playbook_draft_versions WHERE tree_digest IS NULL")" = 0 ] ||
    fail "[B] draft versions are left unconverted"
[ "$(sql -c "SELECT count(*) FROM pack_digest_aliases WHERE unconvertible_reason IS NOT NULL")" = 1 ] ||
    fail "[B] the symlink pack has no unconvertible alias"
TREE=$(sql -c "SELECT tree_digest FROM playbook_draft_versions WHERE draft_id = '$DRAFT' AND version = $VERSION")
[[ "$TREE" =~ ^tree1:[0-9a-f]{64}$ ]] || fail "[B] version $VERSION has tree digest '$TREE'"
sql -c "SELECT path FROM pack_tree_files WHERE digest = '$TREE'" | grep -qx 'nested/deep/marker.txt' ||
    fail "[B] the stored tree is missing nested/deep/marker.txt"
pass "[B] boot $BOOT converted $converted packs and recorded the symlink pack unconvertible"

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
launch_and_check B

echo "kind e2e passed"
