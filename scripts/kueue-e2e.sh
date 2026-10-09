#!/usr/bin/env bash
# End-to-end proof that a sandboxed agent reaches crucible-kueue with an agent token its own
# supervisor obtained, and with nothing else.
#
# A controller from this checkout runs in the cluster (CTL_NS, with its own Postgres) and
# dispatches a playbook launch of e2e/kueue/pack into LOOP_NS, with each run's OpenShell gateway
# isolated in crucible-sandboxes. The engine registers a token-grant provider for the run; the
# sandbox's supervisor fetches its JWT-SVID from SPIRE, exchanges it at the controller's token
# endpoint, and injects the token on traffic to crucible-kueue, which acts as the token's
# kube_user. The script then checks the cluster and the controller, not the agent's word alone:
#   - the controller issued a token to a supervisor SPIFFE ID under this cluster's gateway prefix,
#     for the launcher's kube_user and the run;
#   - the Job exists in the workload namespace, carries that run, launcher and queue, and succeeded;
#   - the agent's whoami reported the same run and kube_user;
#   - no kueue token rode the loop pod's env, its Secrets, or any log.
#
# The launch is made with the caller's own cluster token, so the controller learns the launcher's
# Kubernetes user from it. crucible-system's crucible-kueue Deployment and ClusterRole are saved
# first and restored on exit.
#
# Needs kubectl, jq, openssl, cargo, and on the cluster: Kueue, OpenShift's Zero Trust Workload
# Identity Manager with a ClusterSPIFFEID for supervisors in crucible-sandboxes, the crucible-enmaas
# Secret (the EnMaaS key, under api-key) and the PULL_SECRET Secret in crucible-system.
#
#   CONTEXT=<kube context> LOOP_IMAGE=<runtime image> KUEUE_IMAGE=<crucible-kueue image> \
#   CONTROLLER_IMAGE=<controller image> PULL_SECRET=<secret> scripts/kueue-e2e.sh
#
# LOOP_IMAGE is Containerfile.runtime-selfcontained built from this checkout, CONTROLLER_IMAGE is
# Containerfile.controller-selfcontained on top of it, KUEUE_IMAGE is Containerfile.kueue.
# SANDBOX_SCC_ROLE names a ClusterRole to bind the sandbox account to, where the cluster needs one.
# KEEP=1 leaves everything behind.
set -euo pipefail

: "${CONTEXT:?set CONTEXT to the kube context of the cluster under test}"
: "${LOOP_IMAGE:?set LOOP_IMAGE to a runtime image built from this checkout}"
: "${KUEUE_IMAGE:?set KUEUE_IMAGE to a crucible-kueue image built from this checkout}"
: "${CONTROLLER_IMAGE:?set CONTROLLER_IMAGE to a controller image built from this checkout}"
: "${PULL_SECRET:?set PULL_SECRET to the crucible-system pull Secret for those images}"
CTL_NS=crucible-kueue-e2e-ctl
LOOP_NS=crucible-kueue-e2e-loops
SANDBOX_NS=crucible-sandboxes
WORKLOAD_NS=crucible-kueue-e2e
SPIRE_NS=zero-trust-workload-identity-manager
SPIRE_DISCOVERY=spire-spiffe-oidc-discovery-provider:443
PORT=${PORT:-8890}
JOB=kueue-e2e-hello
TIMEOUT_SECS=${TIMEOUT_SECS:-1800}

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d)
kc() { kubectl --context "$CONTEXT" "$@"; }
log() { printf '[kueue-e2e] %s\n' "$*" >&2; }
fail() { log "FAIL: $*"; exit 1; }

FORWARD_PID=
cleanup() {
    [ -n "$FORWARD_PID" ] && kill "$FORWARD_PID" 2>/dev/null || true
    kc -n "$CTL_NS" logs deploy/crucible-controller > "$WORK/controller.log" 2>/dev/null || true
    if [ "${KEEP:-0}" = 1 ]; then
        log "kept $WORK, the namespaces, and Job $WORKLOAD_NS/$JOB"
        return
    fi
    kc -n "$WORKLOAD_NS" delete job "$JOB" --ignore-not-found >/dev/null || true
    [ -f "$WORK/rendered.yaml" ] && yq 'select(.kind == "ClusterRole" or .kind == "ClusterRoleBinding")' \
        "$WORK/rendered.yaml" | kc delete --ignore-not-found -f - >/dev/null 2>&1 || true
    kc -n "$SPIRE_NS" delete role,rolebinding crucible-kueue-e2e-spire-keys --ignore-not-found >/dev/null || true
    kc delete namespace "$CTL_NS" "$LOOP_NS" "$SANDBOX_NS" --ignore-not-found --wait=false >/dev/null || true
    if [ -s "$WORK/kueue-saved.yaml" ]; then
        kc replace -f "$WORK/kueue-saved.yaml" >/dev/null && log "restored crucible-system's crucible-kueue"
    elif [ -f "$WORK/kueue.yaml" ]; then
        kc delete --ignore-not-found -f "$WORK/kueue.yaml" >/dev/null || true
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT
trap 'log "FAIL: line $LINENO: $BASH_COMMAND"' ERR

log "building crucible and crux"
SQLX_OFFLINE=true cargo build -q --manifest-path "$ROOT/Cargo.toml" -p crucible -p crux --bins
BIN=$(cargo metadata -q --manifest-path "$ROOT/Cargo.toml" --format-version 1 --no-deps \
    | jq -r .target_directory)/debug

LAUNCHER=$(kc auth whoami -o jsonpath='{.status.userInfo.username}')
TOKEN=$(kubectl config view --raw --minify --context "$CONTEXT" -o jsonpath='{.users[0].user.token}')
[ -n "$TOKEN" ] || fail "context $CONTEXT carries no bearer token to launch with"
TRUST_DOMAIN=$(kc get zerotrustworkloadidentitymanager cluster -o jsonpath='{.spec.trustDomain}')
SVID_ISSUER=$(kc get spireoidcdiscoveryprovider cluster -o jsonpath='{.spec.jwtIssuer}')
[ -n "$TRUST_DOMAIN" ] && [ -n "$SVID_ISSUER" ] || fail "no SPIRE trust domain or JWT issuer on the cluster"
GATEWAY_PREFIX="spiffe://$TRUST_DOMAIN/openshell/gateway/"
ISSUER="http://crucible-controller.$CTL_NS.svc.cluster.local:8080"
KUEUE_URL="http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp"
log "launching as $LAUNCHER; SVIDs from $SVID_ISSUER under $GATEWAY_PREFIX"

# The sandbox supervisor must come from the OpenShell rev the engine's gateway client links.
SUPERVISOR_IMAGE=ghcr.io/neuralmagic/openshell-supervisor:sha-$(cd "$ROOT" && cargo xtask openshell-rev)
fill() {
    sed -e "s|@LOOP_IMAGE@|$LOOP_IMAGE|g" -e "s|@KUEUE_IMAGE@|$KUEUE_IMAGE|g" \
        -e "s|@CONTROLLER_IMAGE@|$CONTROLLER_IMAGE|g" -e "s|@SUPERVISOR_IMAGE@|$SUPERVISOR_IMAGE|g" \
        -e "s|@PULL_SECRET@|$PULL_SECRET|g" -e "s|@CTL_NS@|$CTL_NS|g" -e "s|@LOOP_NS@|$LOOP_NS|g" \
        -e "s|@SPIRE_NS@|$SPIRE_NS|g" -e "s|@ISSUER@|$ISSUER|g" -e "s|@KUEUE_URL@|$KUEUE_URL|g" \
        -e "s|@LAUNCHER@|$LAUNCHER|g" \
        -e "s|@AGENT_SPIRE@|hub=$SVID_ISSUER $GATEWAY_PREFIX $SPIRE_NS/$SPIRE_DISCOVERY|g" "$1"
}
fill "$ROOT/e2e/kueue/profile.toml.in" > "$WORK/profile.toml"

log "saving crucible-system's crucible-kueue"
for obj in deployment/crucible-kueue clusterrole/crucible-kueue-cluster; do
    kc -n crucible-system get "$obj" -o json 2>/dev/null \
        | jq 'del(.metadata.resourceVersion, .metadata.uid, .metadata.creationTimestamp, .metadata.generation, .metadata.managedFields, .status)'
done | jq -s '{apiVersion: "v1", kind: "List", items: .}' > "$WORK/kueue-saved.json"
if [ "$(jq '.items | length' "$WORK/kueue-saved.json")" -gt 0 ]; then
    cp "$WORK/kueue-saved.json" "$WORK/kueue-saved.yaml"
fi

log "clearing what an earlier run left"
kc delete namespace "$CTL_NS" "$LOOP_NS" "$SANDBOX_NS" --ignore-not-found --timeout=300s >/dev/null

log "deploying the controller, the namespaces and their RBAC"
fill "$ROOT/e2e/kueue/controller.yaml.in" | kc apply -f - >/dev/null
kc create namespace "$SANDBOX_NS" --dry-run=client -o yaml | kc apply -f - >/dev/null
kc apply -f "$ROOT/e2e/kueue/namespace.yaml" >/dev/null
for ns in "$CTL_NS" "$LOOP_NS" "$SANDBOX_NS"; do
    kc -n crucible-system get secret "$PULL_SECRET" -o json \
        | jq 'del(.metadata.namespace, .metadata.resourceVersion, .metadata.uid, .metadata.creationTimestamp, .metadata.ownerReferences, .metadata.managedFields)' \
        | kc -n "$ns" apply -f - >/dev/null
done
kc -n crucible-system get secret crucible-enmaas -o json \
    | jq 'del(.metadata.namespace, .metadata.resourceVersion, .metadata.uid, .metadata.creationTimestamp, .metadata.ownerReferences, .metadata.managedFields)' \
    | kc -n "$LOOP_NS" apply -f - >/dev/null
kc -n "$LOOP_NS" create serviceaccount crucible-kueue-e2e-loop --dry-run=client -o yaml | kc apply -f - >/dev/null
cat > "$WORK/kubeconfig" <<'KUBECONFIG'
apiVersion: v1
kind: Config
clusters:
  - name: rig
    cluster:
      server: https://kubernetes.default.svc
      certificate-authority: /var/run/kube/ca.crt
contexts:
  - name: rig
    context: {cluster: rig, user: pod}
current-context: rig
users:
  - name: pod
    user: {tokenFile: /var/run/kube/token}
KUBECONFIG
kc -n "$LOOP_NS" create configmap autoresearch-kubeconfig --from-file=kubeconfig="$WORK/kubeconfig" \
    --dry-run=client -o yaml | kc apply -f - >/dev/null
kc -n "$SANDBOX_NS" create serviceaccount crucible-sandbox --dry-run=client -o yaml | kc apply -f - >/dev/null
if [ -n "${SANDBOX_SCC_ROLE:-}" ]; then
    kc -n "$SANDBOX_NS" create rolebinding crucible-sandbox-scc --clusterrole "$SANDBOX_SCC_ROLE" \
        --serviceaccount "$SANDBOX_NS:crucible-sandbox" --dry-run=client -o yaml | kc apply -f - >/dev/null
fi
"$BIN/crucible" deploy render --manifest "$ROOT/e2e/kueue/render.toml" --profile "$WORK/profile.toml" \
    --no-pin 2>/dev/null | yq 'select(.kind != "Pod" and .kind != "NetworkPolicy")' > "$WORK/rendered.yaml"
kc apply -f "$WORK/rendered.yaml" >/dev/null

log "deploying crucible-kueue in crucible-system"
"$BIN/crucible" deploy render --kueue --no-pin --profile "$WORK/profile.toml" 2>/dev/null > "$WORK/kueue.yaml"
kc apply --server-side --force-conflicts -f "$WORK/kueue.yaml" >/dev/null
kc -n "$WORKLOAD_NS" delete job "$JOB" --ignore-not-found >/dev/null

log "starting the controller in $CTL_NS"
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$WORK/agent-signing.pem" 2>/dev/null
head -c 32 /dev/urandom | base64 > "$WORK/webhook.key"
kc -n "$CTL_NS" create secret generic crucible-controller \
    --from-literal=api-token="$(openssl rand -hex 32)" \
    --from-file=agent-signing.pem="$WORK/agent-signing.pem" \
    --from-file=webhook.key="$WORK/webhook.key" --dry-run=client -o yaml | kc apply -f - >/dev/null
kc -n "$CTL_NS" create configmap crucible-controller-profile --from-file=profile.toml="$WORK/profile.toml" \
    --dry-run=client -o yaml | kc apply -f - >/dev/null
kc -n "$CTL_NS" rollout restart deploy/crucible-controller >/dev/null
kc -n crucible-system rollout status deploy/crucible-kueue --timeout=300s >/dev/null
kc -n "$CTL_NS" rollout status deploy/postgres --timeout=300s >/dev/null
kc -n "$CTL_NS" rollout status deploy/crucible-controller --timeout=600s >/dev/null \
    || fail "the controller never became ready: $(kc -n "$CTL_NS" logs deploy/crucible-controller --tail=20)"

kubectl --context "$CONTEXT" -n "$CTL_NS" port-forward svc/crucible-controller "$PORT:8080" \
    > "$WORK/forward.log" 2>&1 &
FORWARD_PID=$!
printf 'url = "http://127.0.0.1:%s"\n' "$PORT" > "$WORK/crux.toml"
crux() { env -u CONTROLLER_URL CONTROLLER_API_TOKEN="$TOKEN" "$BIN/crux" "$@" --config "$WORK/crux.toml"; }
for _ in $(seq 30); do
    crux whoami >/dev/null 2>&1 && break
    sleep 2
done
crux whoami | grep -q "$LAUNCHER" || fail "the controller does not see the launch as $LAUNCHER: $(crux whoami)"

log "launching e2e/kueue/pack"
crux draft-create kueue-e2e --description "crucible-kueue e2e" >/dev/null
crux draft-push kueue-e2e "$ROOT/e2e/kueue/pack" --base-version 1 >/dev/null
crux draft-launch kueue-e2e --max-cost 3 --max-time 30m >/dev/null

log "waiting for Job $WORKLOAD_NS/$JOB to succeed"
deadline=$((SECONDS + TIMEOUT_SECS))
while :; do
    job=$(kc -n "$WORKLOAD_NS" get job "$JOB" -o json 2>/dev/null || true)
    if [ -n "$job" ]; then
        [ "$(jq -r '.status.failed // 0' <<<"$job")" = 0 ] || fail "Job $JOB failed"
        [ "$(jq -r '.status.succeeded // 0' <<<"$job")" -ge 1 ] && break
    fi
    launch=$(crux playbook-runs --json 2>/dev/null | jq -r '.[0] | "\(.status)\t\(.parked_reason // "")"') || launch=
    case ${launch%%$'\t'*} in
        done | parked | failed | finished | cancelled) fail "the run ended (${launch%%$'\t'*}) with no succeeded Job: ${launch#*$'\t'}" ;;
    esac
    [ $SECONDS -lt $deadline ] || fail "Job $JOB did not succeed within ${TIMEOUT_SECS}s"
    sleep 10
done
RUN=$(jq -r '.metadata.labels["crucible.dev/run-id"]' <<<"$job")
[ "$(jq -r '.metadata.labels["crucible.dev/managed-by"]' <<<"$job")" = crucible-kueue ] \
    || fail "Job $JOB was not created by crucible-kueue"
[ "$(jq -r '.metadata.labels["crucible.dev/launched-by"]' <<<"$job")" = "$LAUNCHER" ] \
    || fail "Job $JOB was not launched for $LAUNCHER"
[ "$(jq -r '.metadata.labels["kueue.x-k8s.io/queue-name"]' <<<"$job")" = crucible-e2e ] \
    || fail "Job $JOB was not queued through crucible-e2e"
log "Job $JOB succeeded for run $RUN, launched by $LAUNCHER, admitted through crucible-e2e"

log "checking the loop pod carries no kueue token"
POD=$(kc -n "$LOOP_NS" get pods -o json | jq -r '[.items[] | select(.metadata.labels["crucible/run"])][0].metadata.name')
[ "$POD" != null ] || fail "no loop pod in $LOOP_NS"
kueue_env=$(kc -n "$LOOP_NS" get pod "$POD" -o json \
    | jq -r '[.spec.containers[].env[]?.name | select(test("KUEUE"))] | join(",")')
[ "$kueue_env" = CRUCIBLE_KUEUE_URL ] || fail "loop pod $POD has kueue env $kueue_env"
kc -n "$LOOP_NS" get secrets -o json | jq -e '[.items[].data // {} | keys[] | select(test("KUEUE"))] | length == 0' >/dev/null \
    || fail "a Secret in $LOOP_NS carries a kueue key"

log "waiting for the run to finish"
deadline=$((SECONDS + 900))
until crux playbook-runs --json | jq -e '.[0].status | IN("done", "parked", "failed", "finished", "cancelled")' >/dev/null; do
    [ $SECONDS -lt $deadline ] || fail "the run did not finish"
    sleep 10
done
crux run-log "$RUN" --json > "$WORK/run-log.json" 2>/dev/null || kc -n "$LOOP_NS" logs "$POD" > "$WORK/run-log.json"
kc -n "$CTL_NS" logs deploy/crucible-controller | sed 's/\x1b\[[0-9;]*m//g' > "$WORK/controller.log"

log "checking the controller issued the run's token to its supervisor"
issued=$(grep 'agent token issued' "$WORK/controller.log" | grep -E "run_id=\"?$RUN\"?( |$)" | head -1)
[ -n "$issued" ] || fail "the controller issued no agent token for run $RUN"
grep -qE "kube_user=\"?$LAUNCHER\"?( |$)" <<<"$issued" || fail "the token was not for kube_user $LAUNCHER: $issued"
grep -qE "spiffe_id=\"?${GATEWAY_PREFIX}[a-z0-9-]+/sandbox/" <<<"$issued" \
    || fail "the token went to no supervisor under $GATEWAY_PREFIX: $issued"
log "issued: $issued"

log "checking the agent's whoami"
whoami=$(python3 - "$WORK/run-log.json" <<'EOF'
import json, sys
text = open(sys.argv[1]).read()
decoder = json.JSONDecoder()
for _ in range(3):
    at = text.rfind("WHOAMI")
    while at != -1:
        start = text.find("{", at)
        try:
            found, _ = decoder.raw_decode(text, start)
            print(json.dumps(found))
            sys.exit(0)
        except ValueError:
            at = text.rfind("WHOAMI", 0, at)
    text = text.encode().decode("unicode_escape")
EOF
)
[ -n "$whoami" ] || fail "the agent printed no WHOAMI line; see $WORK/run-log.json"
[ "$(jq -r .run_id <<<"$whoami")" = "$RUN" ] || fail "whoami names another run: $whoami"
[ "$(jq -r .kube_user <<<"$whoami")" = "$LAUNCHER" ] || fail "whoami names another kube_user: $whoami"
log "whoami: $whoami"

if grep -q CRUCIBLE_KUEUE_TOKEN "$WORK/controller.log" "$WORK/run-log.json"; then
    fail "a static kueue token appears in the run's logs"
fi
log "PASS"
