#!/usr/bin/env bash
# End-to-end proof that a sandboxed agent run submits a Kueue Job through crucible-kueue.
#
# A controller on this machine (embedded Postgres, no auth) dispatches a playbook launch of
# e2e/kueue/pack onto the cluster as a spoke. At dispatch it mints the run's token, writes the
# tenant record, and hands the token to the loop pod through the run Secret; the engine binds it
# into the OpenShell sandbox as a provider placeholder; the agent submits a Job through the
# `kueue` MCP server. The script then checks the cluster, not the agent's word: the Job exists in
# the workload namespace, carries the run's label, and succeeded, and collection deleted the
# tenant record.
#
# Needs kubectl, jq, cargo, and on the cluster: Kueue, LocalQueue crucible-e2e (applied here from
# e2e/kueue/namespace.yaml), the hub dispatch RBAC in crucible-system, and the
# crucible-enmaas (the EnMaaS key, under api-key) and quay-wseaton-pull Secrets the profile names.
#
#   CONTEXT=<kube context> LOOP_IMAGE=<runtime image> KUEUE_IMAGE=<crucible-kueue image> \
#     scripts/kueue-e2e.sh
#
# LOOP_IMAGE is Containerfile.runtime-selfcontained built from this checkout (the engine half);
# KUEUE_IMAGE is Containerfile.kueue. KEEP=1 leaves the controller state and the Job behind.
set -euo pipefail

: "${CONTEXT:?set CONTEXT to the kube context of the cluster under test}"
: "${LOOP_IMAGE:?set LOOP_IMAGE to a runtime image built from this checkout}"
: "${KUEUE_IMAGE:?set KUEUE_IMAGE to a crucible-kueue image built from this checkout}"
CLUSTER=pirate
PORT=${PORT:-8890}
JOB=kueue-e2e-hello
WORKLOAD_NS=crucible-kueue-e2e
TENANT_NS=crucible-kueue-tenants
TIMEOUT_SECS=${TIMEOUT_SECS:-1800}

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d)
kc() { kubectl --context "$CONTEXT" "$@"; }
log() { printf '[kueue-e2e] %s\n' "$*" >&2; }
fail() { log "FAIL: $*"; exit 1; }

CONTROLLER_PID=
cleanup() {
    [ -n "$CONTROLLER_PID" ] && kill "$CONTROLLER_PID" 2>/dev/null || true
    if [ "${KEEP:-0}" = 1 ]; then
        log "kept $WORK (controller state, logs) and Job $WORKLOAD_NS/$JOB"
    else
        kc -n "$WORKLOAD_NS" delete job "$JOB" --ignore-not-found >/dev/null || true
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

log "building crucible, crucible-controller, crux"
SQLX_OFFLINE=true cargo build -q --manifest-path "$ROOT/Cargo.toml" \
    -p crucible -p crucible-controller -p crux --bins --features crucible-controller/embedded-db
BIN=$(cargo metadata -q --manifest-path "$ROOT/Cargo.toml" --format-version 1 --no-deps \
    | jq -r .target_directory)/debug

# The sandbox supervisor must come from the OpenShell rev the engine's gateway client links.
SUPERVISOR_IMAGE=ghcr.io/neuralmagic/openshell-supervisor:sha-$(cd "$ROOT" && cargo xtask openshell-rev)
sed -e "s|@LOOP_IMAGE@|$LOOP_IMAGE|" -e "s|@KUEUE_IMAGE@|$KUEUE_IMAGE|" \
    -e "s|@SUPERVISOR_IMAGE@|$SUPERVISOR_IMAGE|" \
    "$ROOT/e2e/kueue/profile.toml.in" > "$WORK/profile.toml"
mkdir -p "$WORK/clusters/$CLUSTER" "$WORK/state"
(umask 077
 kubectl config view --raw --minify --flatten --context "$CONTEXT" > "$WORK/clusters/$CLUSTER/kubeconfig"
 KUBECONFIG="$WORK/clusters/$CLUSTER/kubeconfig" kubectl config set-context --current \
     --namespace crucible-system >/dev/null
 head -c 32 /dev/urandom | base64 > "$WORK/state/webhook.key")

log "deploying the workload namespace and crucible-kueue"
kc apply -f "$ROOT/e2e/kueue/namespace.yaml" >/dev/null
"$BIN/crucible" deploy render --kueue --no-pin --profile "$WORK/profile.toml" 2>/dev/null \
    | kc apply --server-side -f - >/dev/null
kc -n crucible-system rollout status deploy/crucible-kueue --timeout=300s >/dev/null
kc -n "$WORKLOAD_NS" delete job "$JOB" --ignore-not-found >/dev/null

log "starting the controller on 127.0.0.1:$PORT"
env -u CONTROLLER_API_TOKEN -u CONTROLLER_PROXY_TOKEN -u CONTROLLER_OIDC_ISSUER -u VAULT_ADDR \
    KUBECONFIG=/dev/null \
    DATABASE_URL=embedded \
    CONTROLLER_API_ADDR="127.0.0.1:$PORT" \
    CONTROLLER_PUBLIC_URL="http://127.0.0.1:$PORT" \
    CONTROLLER_HOOKS_ADDR="127.0.0.1:$((PORT + 1))" \
    CONTROLLER_HOOKS_PUBLIC_URL="http://127.0.0.1:$((PORT + 1))" \
    CONTROLLER_WEBHOOK_KEY_FILE="$WORK/state/webhook.key" \
    CONTROLLER_DEV_IDENTITY="$USER" \
    CONTROLLER_ADMINS="$USER" \
    CONTROLLER_AUTH_MODE=proxy \
    CONTROLLER_SESSION_SECURE=false \
    CONTROLLER_PLAYBOOK_EXECUTOR=pod \
    CONTROLLER_SCOPE_EXECUTOR=disabled \
    CONTROLLER_DEPLOY_PROFILE="$WORK/profile.toml" \
    CONTROLLER_CLUSTERS_DIR="$WORK/clusters" \
    CONTROLLER_DISPATCH_CLUSTER="$CLUSTER" \
    CONTROLLER_RENDER_NO_PIN=true \
    CONTROLLER_STATE_DIR="$WORK/state" \
    CRUCIBLE_BIN="$BIN/crucible" \
    RUST_LOG=info \
    "$BIN/crucible-controller" autopilot > "$WORK/controller.log" 2>&1 &
CONTROLLER_PID=$!

printf 'url = "http://127.0.0.1:%s"\n' "$PORT" > "$WORK/crux.toml"
crux() { env -u CONTROLLER_API_TOKEN -u CONTROLLER_URL "$BIN/crux" "$@" --config "$WORK/crux.toml"; }
for _ in $(seq 60); do
    crux whoami >/dev/null 2>&1 && break
    kill -0 "$CONTROLLER_PID" 2>/dev/null || fail "controller exited; see $WORK/controller.log"
    sleep 2
done
crux whoami >/dev/null || fail "controller never answered"

log "launching e2e/kueue/pack onto $CLUSTER"
crux draft-create kueue-e2e --description "crucible-kueue e2e" >/dev/null
crux draft-push kueue-e2e "$ROOT/e2e/kueue/pack" --base-version 1 >/dev/null
crux draft-launch kueue-e2e --max-cost 3 --max-time 30m --dispatch-target "$CLUSTER" >/dev/null

log "waiting for the run's tenant record"
deadline=$((SECONDS + 300))
RECORD=
until [ -n "$RECORD" ]; do
    RECORD=$(kc -n "$TENANT_NS" get cm -l crucible.dev/kueue-tenant=true -o name | head -1)
    [ $SECONDS -lt $deadline ] || fail "no tenant record in $TENANT_NS"
    [ -n "$RECORD" ] || sleep 5
done
RUN=$(kc -n "$TENANT_NS" get "$RECORD" -o json | jq -r '.data["tenant.json"] | fromjson | .run_id')
log "record $RECORD grants run $RUN"

log "waiting for Job $WORKLOAD_NS/$JOB to succeed"
deadline=$((SECONDS + TIMEOUT_SECS))
while :; do
    job=$(kc -n "$WORKLOAD_NS" get job "$JOB" -o json 2>/dev/null || true)
    if [ -n "$job" ]; then
        [ "$(jq -r '.status.failed // 0' <<<"$job")" = 0 ] || fail "Job $JOB failed"
        [ "$(jq -r '.status.succeeded // 0' <<<"$job")" -ge 1 ] && break
    fi
    kill -0 "$CONTROLLER_PID" 2>/dev/null || fail "controller exited; see $WORK/controller.log"
    launch=$(crux playbook-runs --json | jq -r '.[0] | "\(.status)\t\(.parked_reason // "")"')
    case ${launch%%$'\t'*} in
        done | parked | failed | finished | cancelled) fail "the run ended (${launch%%$'\t'*}) with no succeeded Job: ${launch#*$'\t'}" ;;
    esac
    [ $SECONDS -lt $deadline ] || fail "Job $JOB did not succeed within ${TIMEOUT_SECS}s"
    sleep 10
done

[ "$(jq -r '.metadata.labels["crucible.dev/run-id"]' <<<"$job")" = "$RUN" ] \
    || fail "Job $JOB is not labelled with run $RUN"
[ "$(jq -r '.metadata.labels["crucible.dev/managed-by"]' <<<"$job")" = crucible-kueue ] \
    || fail "Job $JOB was not created by crucible-kueue"
[ "$(jq -r '.metadata.labels["kueue.x-k8s.io/queue-name"]' <<<"$job")" = crucible-e2e ] \
    || fail "Job $JOB was not queued through crucible-e2e"
log "Job $JOB succeeded, labelled for run $RUN, admitted through crucible-e2e"

log "waiting for collection to delete the tenant record"
deadline=$((SECONDS + 900))
while kc -n "$TENANT_NS" get "$RECORD" >/dev/null 2>&1; do
    [ $SECONDS -lt $deadline ] || fail "tenant record $RECORD outlived its run"
    sleep 10
done
log "PASS"
