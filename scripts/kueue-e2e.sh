#!/usr/bin/env bash
# End-to-end proof that a sandboxed agent builds an image with the run's buildit and runs it
# through crucible-kueue, with agent tokens its own supervisor obtained and with nothing else.
#
# A controller from this checkout runs in the cluster (CTL_NS, with its own Postgres) and
# dispatches a playbook launch of e2e/kueue/pack into LOOP_NS, with each run's OpenShell gateway
# isolated in SANDBOX_NS and a crucible-kueue server of its own in KUEUE_NS. The engine starts
# buildit in the loop pod, with its builder pods in BUILD_NS, and registers a token-grant provider for it and for crucible-kueue; the
# sandbox's supervisor fetches its JWT-SVID from SPIRE, exchanges it at the controller's token
# endpoint, and injects each token on traffic to its server alone. The agent writes a program printing a nonce, builds and pushes it to
# PUSH_REPO, and runs it as a Job. The script then checks the cluster and the controller, not the
# agent's word alone:
#   - the controller issued buildit and crucible-kueue tokens to a supervisor SPIFFE ID under this
#     cluster's gateway prefix, for the launcher's kube_user and the run;
#   - the Job exists in the workload namespace, carries that run, launcher and queue, runs an
#     image pushed to PUSH_REPO by digest, succeeded, and its log carries the nonce;
#   - the agent's whoami reported the same run and kube_user;
#   - no kueue token rode the loop pod's env, its Secrets, or any log.
#
# The launch is made with the caller's own cluster token, and the launcher's Kubernetes user is
# seeded in the controller's ledger as a hub link would record it.
#
# The e2e runs beside a live deployment on the same cluster: everything it creates lives in its own
# namespaces or carries an e2e name, the cluster-scoped objects (its ClusterRoles, bindings and a
# ClusterSPIFFEID for SANDBOX_NS) carry the label crucible.dev/e2e=kueue, and teardown deletes by
# that label alone. It refuses to start when one of its namespaces exists without the label.
#
# Needs kubectl, jq, yq, openssl, cargo, and on the cluster: Kueue with a ClusterQueue crucible,
# OpenShift's Zero Trust Workload Identity Manager, the ANTHROPIC_KEY_SECRET Secret (an Anthropic Messages API key for ANTHROPIC_BASE_URL, under api-key)
# and the PULL_SECRET Secret in crucible-system, whose account may also push to PUSH_REPO.
#
#   CONTEXT=<kube context> LOOP_IMAGE=<runtime image> KUEUE_IMAGE=<crucible-kueue image> \
#   CONTROLLER_IMAGE=<controller image> PULL_SECRET=<secret> PUSH_REPO=<registry/path> \
#   ANTHROPIC_BASE_URL=<url> ANTHROPIC_KEY_SECRET=<secret> scripts/kueue-e2e.sh
#
# LOOP_IMAGE is Containerfile.runtime-selfcontained built from this checkout, CONTROLLER_IMAGE is
# Containerfile.controller-selfcontained on top of it, KUEUE_IMAGE is Containerfile.kueue.
# SANDBOX_SCC_ROLE and LOOP_SCC_ROLE name ClusterRoles to bind the sandbox and loop accounts to,
# where the cluster needs them (an SCC giving the loop pod a writable home, on OpenShift).
# KEEP=1 leaves everything behind.
set -euo pipefail

: "${CONTEXT:?set CONTEXT to the kube context of the cluster under test}"
: "${LOOP_IMAGE:?set LOOP_IMAGE to a runtime image built from this checkout}"
: "${KUEUE_IMAGE:?set KUEUE_IMAGE to a crucible-kueue image built from this checkout}"
: "${CONTROLLER_IMAGE:?set CONTROLLER_IMAGE to a controller image built from this checkout}"
: "${PULL_SECRET:?set PULL_SECRET to the crucible-system pull Secret for those images}"
: "${PUSH_REPO:?set PUSH_REPO to the repository the pack pushes its build to, which PULL_SECRET may push to}"
: "${ANTHROPIC_BASE_URL:?set ANTHROPIC_BASE_URL to the Anthropic Messages API endpoint the agent uses}"
: "${ANTHROPIC_KEY_SECRET:?set ANTHROPIC_KEY_SECRET to the crucible-system Secret holding its api-key}"
CTL_NS=crucible-kueue-e2e-ctl
LOOP_NS=crucible-kueue-e2e-loops
KUEUE_NS=crucible-kueue-e2e-server
BUILD_NS=crucible-kueue-e2e-builds
SANDBOX_NS=${SANDBOX_NS:-crucible-kueue-e2e-sandboxes}
WORKLOAD_NS=crucible-kueue-e2e-workloads
E2E_LABEL=crucible.dev/e2e=kueue
SPIRE_NS=zero-trust-workload-identity-manager
SPIRE_DISCOVERY=spire-spiffe-oidc-discovery-provider:443
NONCE=$(openssl rand -hex 12)
JOB=crucible-smoke-${NONCE:0:8}
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
    teardown --wait=false
    rm -rf "$WORK"
}
teardown() {
    kc delete clusterrolebinding,clusterrole,clusterspiffeid -l "$E2E_LABEL" --ignore-not-found >/dev/null || true
    kc -n "$SPIRE_NS" delete rolebinding,role -l "$E2E_LABEL" --ignore-not-found >/dev/null || true
    kc delete namespace -l "$E2E_LABEL" --ignore-not-found "$@" >/dev/null || true
}
# Name a render's cluster-scoped objects for the e2e and label them, and point bindings at the
# renamed ClusterRoles, so nothing collides with a live deployment's objects of the same name.
e2e_scoped() {
    yq -o=json -I=0 '.' "$1" | jq -s '
        ([.[] | select(.kind == "ClusterRole") | .metadata.name]) as $roles
        | {apiVersion: "v1", kind: "List", items: [.[] | select(. != null)
            | if (.kind | startswith("Cluster")) then
                .metadata.name += "-kueue-e2e" | .metadata.labels["crucible.dev/e2e"] = "kueue"
              else . end
            | if (.roleRef.kind == "ClusterRole" and (.roleRef.name as $n | $roles | index($n))) then
                .roleRef.name += "-kueue-e2e"
              else . end]}'
}
for ns in "$CTL_NS" "$LOOP_NS" "$KUEUE_NS" "$BUILD_NS" "$SANDBOX_NS" "$WORKLOAD_NS"; do
    if kc get namespace "$ns" >/dev/null 2>&1 \
        && [ "$(kc get namespace "$ns" -o jsonpath='{.metadata.labels.crucible\.dev/e2e}')" != kueue ]; then
        log "FAIL: $ns exists and is not this e2e's"
        exit 1
    fi
done
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
AVOID_NODES=$(kc get nodes -l 'kubernetes.io/arch!=amd64' -o json | jq -r '[.items[].metadata.name | "\"\(.)\""] | join(", ")')
ISSUER="http://crucible-controller.$CTL_NS.svc.cluster.local:8080"
KUEUE_URL="http://crucible-kueue.$KUEUE_NS.svc.cluster.local:8850/mcp"
log "launching as $LAUNCHER; SVIDs from $SVID_ISSUER under $GATEWAY_PREFIX"

# The sandbox supervisor must come from the OpenShell rev the engine's gateway client links.
SUPERVISOR_IMAGE=ghcr.io/neuralmagic/openshell-supervisor:sha-$(cd "$ROOT" && cargo xtask openshell-rev)
fill() {
    sed -e "s|@LOOP_IMAGE@|$LOOP_IMAGE|g" -e "s|@KUEUE_IMAGE@|$KUEUE_IMAGE|g" \
        -e "s|@CONTROLLER_IMAGE@|$CONTROLLER_IMAGE|g" -e "s|@SUPERVISOR_IMAGE@|$SUPERVISOR_IMAGE|g" \
        -e "s|@PULL_SECRET@|$PULL_SECRET|g" -e "s|@CTL_NS@|$CTL_NS|g" -e "s|@LOOP_NS@|$LOOP_NS|g" \
        -e "s|@SANDBOX_NS@|$SANDBOX_NS|g" -e "s|@WORKLOAD_NS@|$WORKLOAD_NS|g" -e "s|@KUEUE_NS@|$KUEUE_NS|g" \
        -e "s|@BUILD_NS@|$BUILD_NS|g" \
        -e "s|@SPIRE_NS@|$SPIRE_NS|g" -e "s|@ISSUER@|$ISSUER|g" -e "s|@KUEUE_URL@|$KUEUE_URL|g" \
        -e "s|@LAUNCHER@|$LAUNCHER|g" -e "s|@AVOID_NODES@|$AVOID_NODES|g" \
        -e "s|@PUSH_REPO@|$PUSH_REPO|g" -e "s|@ANTHROPIC_BASE_URL@|$ANTHROPIC_BASE_URL|g" \
        -e "s|@ANTHROPIC_KEY_SECRET@|$ANTHROPIC_KEY_SECRET|g" -e "s|@NONCE@|$NONCE|g" -e "s|@NONCE_SHORT@|${NONCE:0:8}|g" \
        -e "s|@AGENT_SPIRE@|hub=$SVID_ISSUER $GATEWAY_PREFIX $SPIRE_NS/$SPIRE_DISCOVERY|g" "$1"
}
fill "$ROOT/e2e/kueue/profile.toml.in" > "$WORK/profile.toml"

log "clearing what an earlier run left"
teardown --timeout=300s
for ns in "$CTL_NS" "$LOOP_NS" "$KUEUE_NS" "$BUILD_NS" "$SANDBOX_NS"; do
    kc create namespace "$ns" --dry-run=client -o yaml | kc apply -f - >/dev/null
    kc label namespace "$ns" "$E2E_LABEL" --overwrite >/dev/null
done

log "deploying the controller, the namespaces and their RBAC"
fill "$ROOT/e2e/kueue/controller.yaml.in" | kc apply -f - >/dev/null
fill "$ROOT/e2e/kueue/cluster.yaml.in" | kc apply -f - >/dev/null
for ns in "$CTL_NS" "$LOOP_NS" "$KUEUE_NS" "$SANDBOX_NS" "$WORKLOAD_NS"; do
    kc -n crucible-system get secret "$PULL_SECRET" -o json \
        | jq 'del(.metadata.namespace, .metadata.resourceVersion, .metadata.uid, .metadata.creationTimestamp, .metadata.ownerReferences, .metadata.managedFields)' \
        | kc -n "$ns" apply -f - >/dev/null
done
kc -n crucible-system get secret "$ANTHROPIC_KEY_SECRET" -o json \
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
if [ -n "${LOOP_SCC_ROLE:-}" ]; then
    kc -n "$LOOP_NS" create rolebinding crucible-kueue-e2e-loop-scc --clusterrole "$LOOP_SCC_ROLE" \
        --serviceaccount "$LOOP_NS:crucible-kueue-e2e-loop" --dry-run=client -o yaml | kc apply -f - >/dev/null
fi
if [ -n "${SANDBOX_SCC_ROLE:-}" ]; then
    kc -n "$SANDBOX_NS" create rolebinding crucible-sandbox-scc --clusterrole "$SANDBOX_SCC_ROLE" \
        --serviceaccount "$SANDBOX_NS:crucible-sandbox" --dry-run=client -o yaml | kc apply -f - >/dev/null
fi
"$BIN/crucible" deploy render --manifest "$ROOT/e2e/kueue/render.toml" --profile "$WORK/profile.toml" \
    --no-pin 2>/dev/null | yq 'select(.kind != "Pod" and .kind != "NetworkPolicy")' > "$WORK/rendered.yaml"
e2e_scoped "$WORK/rendered.yaml" | kc apply -f - >/dev/null

log "deploying crucible-kueue in $KUEUE_NS"
"$BIN/crucible" deploy render --kueue --no-pin --profile "$WORK/profile.toml" 2>/dev/null \
    | yq 'select(.kind != "Namespace")' > "$WORK/kueue.yaml"
e2e_scoped "$WORK/kueue.yaml" | kc apply -f - >/dev/null

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
kc -n "$KUEUE_NS" rollout status deploy/crucible-kueue --timeout=300s >/dev/null
kc -n "$CTL_NS" rollout status deploy/postgres --timeout=300s >/dev/null
kc -n "$CTL_NS" rollout status deploy/crucible-controller --timeout=600s >/dev/null \
    || fail "the controller never became ready: $(kc -n "$CTL_NS" logs deploy/crucible-controller --tail=20)"

kubectl --context "$CONTEXT" -n "$CTL_NS" port-forward svc/crucible-controller :8080 \
    > "$WORK/forward.log" 2>&1 &
FORWARD_PID=$!
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
crux whoami | grep -q "$LAUNCHER" || fail "the controller does not see the launch as $LAUNCHER: $(crux whoami)"
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
kc -n "$CTL_NS" exec deploy/postgres -- psql -q -U crucible -d crucible -v ON_ERROR_STOP=1 -c \
    "INSERT INTO users (sub, login, last_login, created_at, updated_at)
     VALUES ('e2e:$LAUNCHER', '$LAUNCHER', '$NOW', '$NOW', '$NOW') ON CONFLICT (login) DO NOTHING;
     INSERT INTO kube_identity_links (sub, cluster, kube_user, source, linked_at)
     SELECT sub, 'hub', '$LAUNCHER', 'token-review', '$NOW' FROM users WHERE login = '$LAUNCHER'
     ON CONFLICT (sub, cluster) DO UPDATE SET kube_user = EXCLUDED.kube_user" >/dev/null

log "launching e2e/kueue/pack with nonce $NONCE"
cp -R "$ROOT/e2e/kueue/pack" "$WORK/pack"
fill "$ROOT/e2e/kueue/pack/prompts/smoke.md" > "$WORK/pack/prompts/smoke.md"
crux draft-create kueue-e2e --description "build-and-run e2e" >/dev/null
crux draft-push kueue-e2e "$WORK/pack" --base-version 1 >/dev/null
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
IMAGE=$(jq -r '.spec.template.spec.containers[0].image' <<<"$job")
[[ $IMAGE == "$PUSH_REPO@sha256:"* ]] || fail "Job $JOB runs $IMAGE, not an image pushed to $PUSH_REPO by digest"
[ "$(jq -c '[.spec.template.spec.imagePullSecrets[]?.name]' <<<"$job")" = "[\"$PULL_SECRET\"]" ] \
    || fail "Job $JOB pulls with $(jq -c .spec.template.spec.imagePullSecrets <<<"$job")"
kc -n "$WORKLOAD_NS" logs "job/$JOB" | grep -qx "crucible-smoke-ok $NONCE" \
    || fail "Job $JOB's log does not carry the nonce: $(kc -n "$WORKLOAD_NS" logs "job/$JOB" | tail -5)"
log "Job $JOB ran $IMAGE and printed the nonce"

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
CRUN=$(crux runs --json | jq -r '.[0].run_id')
crux run-log "$CRUN" > "$WORK/run-log.json"
kc -n "$CTL_NS" logs deploy/crucible-controller | sed 's/\x1b\[[0-9;]*m//g' > "$WORK/controller.log"

log "checking the controller issued the run's tokens to its supervisor"
for aud in "$KUEUE_URL" "http://host.openshell.internal:8848/mcp"; do
    issued=$(grep 'agent token issued' "$WORK/controller.log" | grep "gateway/$RUN/sandbox/" \
        | grep -F "aud=\"$aud\"" | head -1)
    [ -n "$issued" ] || fail "the controller issued no token for $aud to run $RUN"
    grep -qE "kube_user=\"?$LAUNCHER\"?( |$)" <<<"$issued" || fail "the token was not for kube_user $LAUNCHER: $issued"
    grep -qE "spiffe_id=\"?${GATEWAY_PREFIX}$RUN/sandbox/" <<<"$issued" \
        || fail "the token went to no supervisor under $GATEWAY_PREFIX: $issued"
    log "issued: $issued"
done

log "checking the agent's whoami"
whoami=$(python3 - "$WORK/run-log.json" <<'EOF'
import json, sys
text = open(sys.argv[1]).read()
decoder = json.JSONDecoder()
for _ in range(3):
    for marker in ("WHOAMI", '"whoami":'):
        at = text.rfind(marker)
        while at != -1:
            start = text.find("{", at)
            try:
                found, _ = decoder.raw_decode(text, start)
                print(json.dumps(found))
                sys.exit(0)
            except ValueError:
                at = text.rfind(marker, 0, at)
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
