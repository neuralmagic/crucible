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
# Needs kubectl, jq, yq, openssl, cargo, and on the cluster: Kueue with ClusterQueue CLUSTER_QUEUE
# (default crucible), SPIRE with spire-controller-manager, and the PULL_SECRET Secret in
# crucible-system, whose account may also push to PUSH_REPO. The Anthropic Messages API key for
# ANTHROPIC_BASE_URL comes from the crucible-system Secret ANTHROPIC_KEY_SECRET (key api-key), or
# from the local file ANTHROPIC_KEY_FILE, which the script loads into a Secret of its own.
#
# It runs on OpenShift and on plain Kubernetes. PLATFORM (openshift or kubernetes) defaults to
# what API discovery finds: OpenShift serves security.openshift.io (its SCCs). Each value below
# defaults by platform and can be set on its own:
#   SPIRE_NS         zero-trust-workload-identity-manager | spire-server
#   SPIRE_DISCOVERY  <SPIRE_NS>/spire-spiffe-oidc-discovery-provider:443 |
#                    http://<SPIRE_NS>/spire-spiffe-oidc-discovery-provider:80
#   TRUST_DOMAIN, SVID_ISSUER   the Zero Trust Workload Identity Manager's resources | the
#                    spire-server ConfigMap's server.trust_domain and server.jwt_issuer
#   SPIRE_CLASS      zero-trust-workload-identity-manager-spire | spire-controller-manager's
#                    className, which a ClusterSPIFFEID must name when it watches no classless ones
#   *_PSA            on Kubernetes, the Pod Security level each namespace enforces
# On OpenShift the SCC roles below admit the e2e's pods; on Kubernetes its namespaces get Pod
# Security labels instead, privileged only for the builder namespace, where the builder admission
# policy is the guard (the script checks it refuses an escape on either platform).
#
#   CONTEXT=<kube context> LOOP_IMAGE=<runtime image> KUEUE_IMAGE=<crucible-kueue image> \
#   CONTROLLER_IMAGE=<controller image> PULL_SECRET=<secret> PUSH_REPO=<registry/path> \
#   ANTHROPIC_BASE_URL=<url> ANTHROPIC_KEY_SECRET=<secret> scripts/kueue-e2e.sh
#
# LOOP_IMAGE is Containerfile.runtime-selfcontained built from this checkout, CONTROLLER_IMAGE is
# Containerfile.controller-selfcontained on top of it, KUEUE_IMAGE is Containerfile.kueue.
# On OpenShift, SANDBOX_SCC_ROLE and LOOP_SCC_ROLE name ClusterRoles to bind the sandbox and loop
# accounts to (an SCC giving the loop pod a writable home).
# BUILDER_SCC_ROLE (default LOOP_SCC_ROLE) is bound to BUILD_NS's default account, which buildit's
# builder pods run as; the builder admission policy still bounds what those pods may be.
# PACK names the pack to launch, default e2e/kueue/pack (the smoke pack above); MAX_COST, MAX_TIME
# and TIMEOUT_SECS bound it (3, 30m, 1800), and PARAMS (a JSON object of strings) is passed as its
# --params. Prompts that carry @NONCE@ or the other placeholders get them filled. The Job, nonce
# and whoami checks are the smoke pack's. Any other pack passes when its run left at least one Job
# in the workload namespace and the plan's RESULT_TASK (default verify) ended pass. Every pack gets
# the token, isolation and leak checks.
# KEEP=1 leaves everything behind.
set -euo pipefail

: "${CONTEXT:?set CONTEXT to the kube context of the cluster under test}"
: "${LOOP_IMAGE:?set LOOP_IMAGE to a runtime image built from this checkout}"
: "${KUEUE_IMAGE:?set KUEUE_IMAGE to a crucible-kueue image built from this checkout}"
: "${CONTROLLER_IMAGE:?set CONTROLLER_IMAGE to a controller image built from this checkout}"
: "${PULL_SECRET:?set PULL_SECRET to the crucible-system pull Secret for those images}"
: "${PUSH_REPO:?set PUSH_REPO to the repository the pack pushes its build to, which PULL_SECRET may push to}"
: "${ANTHROPIC_BASE_URL:?set ANTHROPIC_BASE_URL to the Anthropic Messages API endpoint the agent uses}"
: "${ANTHROPIC_KEY_SECRET:=}" "${ANTHROPIC_KEY_FILE:=}"
[ -n "$ANTHROPIC_KEY_SECRET$ANTHROPIC_KEY_FILE" ] \
    || { echo "set ANTHROPIC_KEY_SECRET (a crucible-system Secret with api-key) or ANTHROPIC_KEY_FILE" >&2; exit 1; }
[ -z "$ANTHROPIC_KEY_FILE" ] || ANTHROPIC_KEY_SECRET=crucible-kueue-e2e-anthropic
CTL_NS=crucible-kueue-e2e-ctl
LOOP_NS=crucible-kueue-e2e-loops
KUEUE_NS=crucible-kueue-e2e-server
BUILD_NS=crucible-kueue-e2e-builds
SANDBOX_NS=${SANDBOX_NS:-crucible-kueue-e2e-sandboxes}
WORKLOAD_NS=crucible-kueue-e2e-workloads
E2E_LABEL=crucible.dev/e2e=kueue
CLUSTER_QUEUE=${CLUSTER_QUEUE:-crucible}
MAX_COST=${MAX_COST:-3}
MAX_TIME=${MAX_TIME:-30m}
PARAMS=${PARAMS:-}
RESULT_TASK=${RESULT_TASK:-verify}
CTL_PSA=${CTL_PSA:-baseline}
KUEUE_PSA=${KUEUE_PSA:-restricted}
LOOP_PSA=${LOOP_PSA:-baseline}
SANDBOX_PSA=${SANDBOX_PSA:-baseline}
WORKLOAD_PSA=${WORKLOAD_PSA:-baseline}
NONCE=$(openssl rand -hex 12)
JOB=crucible-smoke-${NONCE:0:8}
TIMEOUT_SECS=${TIMEOUT_SECS:-1800}

ROOT=$(cd "$(dirname "$0")/.." && pwd)
PACK=$(cd "${PACK:-$ROOT/e2e/kueue/pack}" && pwd)
SMOKE=0
[ "$PACK" = "$ROOT/e2e/kueue/pack" ] && SMOKE=1
WORK=$(mktemp -d)
kc() { kubectl --context "$CONTEXT" "$@"; }
log() { printf '[kueue-e2e] %s\n' "$*" >&2; }
# A name as crucible-kueue labels it: itself when it is a label value, else its label-safe
# characters and a hash of the whole (an email username, on clusters whose OIDC names people so).
label_safe() {
    if [[ $1 =~ ^[A-Za-z0-9]([A-Za-z0-9._-]{0,61}[A-Za-z0-9])?$ ]]; then
        printf '%s' "$1"
        return
    fi
    local hash kept
    hash=$(printf '%s' "$1" | openssl dgst -sha256 -r | cut -c1-10)
    kept=$(printf '%s' "$1" | tr -c 'A-Za-z0-9._-' '-' | cut -c1-52 | sed -E 's/^[^A-Za-z0-9]+//; s/[^A-Za-z0-9]+$//')
    if [ -n "$kept" ]; then printf '%s-%s' "$kept" "$hash"; else printf '%s' "$hash"; fi
}
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
    kc delete clusterrolebinding,clusterrole,clusterspiffeid,validatingadmissionpolicybinding,validatingadmissionpolicy \
        -l "$E2E_LABEL" --ignore-not-found >/dev/null || true
    [ -z "${SPIRE_NS:-}" ] || kc -n "$SPIRE_NS" delete rolebinding,role -l "$E2E_LABEL" --ignore-not-found >/dev/null || true
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
              elif (.kind | startswith("ValidatingAdmissionPolicy")) then
                .metadata.labels["crucible.dev/e2e"] = "kueue"
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

if [ -z "${PLATFORM:-}" ]; then
    PLATFORM=kubernetes
    kc get --raw /apis/security.openshift.io >/dev/null 2>&1 && PLATFORM=openshift
fi
case $PLATFORM in
    openshift)
        : "${SPIRE_NS:=zero-trust-workload-identity-manager}"
        : "${SPIRE_DISCOVERY:=$SPIRE_NS/spire-spiffe-oidc-discovery-provider:443}"
        : "${SPIRE_CLASS:=zero-trust-workload-identity-manager-spire}"
        : "${TRUST_DOMAIN:=$(kc get zerotrustworkloadidentitymanager cluster -o jsonpath='{.spec.trustDomain}' 2>/dev/null || true)}"
        : "${SVID_ISSUER:=$(kc get spireoidcdiscoveryprovider cluster -o jsonpath='{.spec.jwtIssuer}' 2>/dev/null || true)}"
        SPIRE_SOURCE="the zerotrustworkloadidentitymanager and spireoidcdiscoveryprovider named cluster"
        ;;
    kubernetes)
        : "${SPIRE_NS:=spire-server}"
        : "${SPIRE_DISCOVERY:=http://$SPIRE_NS/spire-spiffe-oidc-discovery-provider:80}"
        server_conf=$(kc -n "$SPIRE_NS" get configmap spire-server -o jsonpath='{.data.server\.conf}' 2>/dev/null || true)
        [ -n "$server_conf" ] || server_conf='{}'
        : "${TRUST_DOMAIN:=$(jq -r '.server.trust_domain // empty' <<<"$server_conf")}"
        : "${SVID_ISSUER:=$(jq -r '.server.jwt_issuer // empty' <<<"$server_conf")}"
        : "${SPIRE_CLASS:=$(kc -n "$SPIRE_NS" get configmap spire-controller-manager \
            -o jsonpath='{.data.controller-manager-config\.yaml}' 2>/dev/null | yq '.className // ""')}"
        SPIRE_SOURCE="ConfigMap $SPIRE_NS/spire-server (server.trust_domain, server.jwt_issuer)"
        ;;
    *) fail "PLATFORM is $PLATFORM, not openshift or kubernetes" ;;
esac
[ -n "$TRUST_DOMAIN" ] || fail "no SPIRE trust domain in $SPIRE_SOURCE; set TRUST_DOMAIN"
[ -n "$SVID_ISSUER" ] || fail "no SPIRE JWT issuer in $SPIRE_SOURCE; set SVID_ISSUER"
[ -n "$SPIRE_CLASS" ] || fail "no spire-controller-manager className in $SPIRE_NS; set SPIRE_CLASS"
if kc get --raw /apis/kueue.x-k8s.io/v1beta2 >/dev/null 2>&1; then KUEUE_API=v1beta2; else KUEUE_API=v1beta1; fi
kc get clusterqueue "$CLUSTER_QUEUE" >/dev/null 2>&1 || fail "no ClusterQueue $CLUSTER_QUEUE; set CLUSTER_QUEUE"

LAUNCHER=$(kc auth whoami -o jsonpath='{.status.userInfo.username}')
# The launcher's own cluster token: the kubeconfig's static token, or what its exec credential
# plugin returns.
TOKEN=$(kubectl config view --raw --minify --context "$CONTEXT" -o jsonpath='{.users[0].user.token}')
if [ -z "$TOKEN" ]; then
    exec_cfg=$(kubectl config view --raw --minify --context "$CONTEXT" -o json | jq -c '.users[0].user.exec // empty')
    if [ -n "$exec_cfg" ]; then
        exec_cmd=() exec_env=()
        while IFS= read -r arg; do exec_cmd+=("$arg"); done < <(jq -r '.command, (.args // [])[]' <<<"$exec_cfg")
        while IFS= read -r var; do exec_env+=("$var"); done < <(jq -r '(.env // [])[] | "\(.name)=\(.value)"' <<<"$exec_cfg")
        exec_info=$(jq -cn --arg v "$(jq -r .apiVersion <<<"$exec_cfg")" \
            '{apiVersion: $v, kind: "ExecCredential", spec: {interactive: false}}')
        TOKEN=$(env ${exec_env[@]+"${exec_env[@]}"} KUBERNETES_EXEC_INFO="$exec_info" "${exec_cmd[@]}" \
            | jq -r '.status.token // empty')
    fi
fi
[ -n "$TOKEN" ] || fail "context $CONTEXT carries no bearer token, nor an exec plugin that returns one"
GATEWAY_PREFIX="spiffe://$TRUST_DOMAIN/openshell/gateway/"
AVOID_NODES=$(kc get nodes -l 'kubernetes.io/arch!=amd64' -o json | jq -r '[.items[].metadata.name | "\"\(.)\""] | join(", ")')
ISSUER="http://crucible-controller.$CTL_NS.svc.cluster.local:8080"
KUEUE_URL="http://crucible-kueue.$KUEUE_NS.svc.cluster.local:8850/mcp"
log "on $PLATFORM, launching as $LAUNCHER; SVIDs from $SVID_ISSUER under $GATEWAY_PREFIX (class $SPIRE_CLASS)"

# The sandbox supervisor must come from the OpenShell rev the engine's gateway client links.
SUPERVISOR_IMAGE=ghcr.io/neuralmagic/openshell-supervisor:sha-$(cd "$ROOT" && cargo xtask openshell-rev)
fill() {
    sed -e "s|@LOOP_IMAGE@|$LOOP_IMAGE|g" -e "s|@KUEUE_IMAGE@|$KUEUE_IMAGE|g" \
        -e "s|@CONTROLLER_IMAGE@|$CONTROLLER_IMAGE|g" -e "s|@SUPERVISOR_IMAGE@|$SUPERVISOR_IMAGE|g" \
        -e "s|@PULL_SECRET@|$PULL_SECRET|g" -e "s|@CTL_NS@|$CTL_NS|g" -e "s|@LOOP_NS@|$LOOP_NS|g" \
        -e "s|@SANDBOX_NS@|$SANDBOX_NS|g" -e "s|@WORKLOAD_NS@|$WORKLOAD_NS|g" -e "s|@KUEUE_NS@|$KUEUE_NS|g" \
        -e "s|@BUILD_NS@|$BUILD_NS|g" -e "s|@PLATFORM@|$PLATFORM|g" -e "s|@SPIRE_CLASS@|$SPIRE_CLASS|g" \
        -e "s|@CLUSTER_QUEUE@|$CLUSTER_QUEUE|g" -e "s|@KUEUE_API@|$KUEUE_API|g" \
        -e "s|@SPIRE_NS@|$SPIRE_NS|g" -e "s|@ISSUER@|$ISSUER|g" -e "s|@KUEUE_URL@|$KUEUE_URL|g" \
        -e "s|@LAUNCHER@|$LAUNCHER|g" -e "s|@AVOID_NODES@|$AVOID_NODES|g" \
        -e "s|@PUSH_REPO@|$PUSH_REPO|g" -e "s|@ANTHROPIC_BASE_URL@|$ANTHROPIC_BASE_URL|g" \
        -e "s|@ANTHROPIC_KEY_SECRET@|$ANTHROPIC_KEY_SECRET|g" -e "s|@NONCE@|$NONCE|g" -e "s|@NONCE_SHORT@|${NONCE:0:8}|g" \
        -e "s|@AGENT_SPIRE@|hub=$SVID_ISSUER $GATEWAY_PREFIX $SPIRE_DISCOVERY|g" "$1"
}
fill "$ROOT/e2e/kueue/profile.toml.in" > "$WORK/profile.toml"

log "clearing what an earlier run left"
teardown --timeout=300s
for ns in "$CTL_NS" "$LOOP_NS" "$KUEUE_NS" "$BUILD_NS" "$SANDBOX_NS"; do
    kc create namespace "$ns" --dry-run=client -o yaml | kc apply -f - >/dev/null
    kc label namespace "$ns" "$E2E_LABEL" --overwrite >/dev/null
done
# Pod admission on plain Kubernetes is Pod Security alone: each namespace enforces the most
# restrictive level its pods pass. buildit's builder pods need AppArmor unconfined, so the builder
# namespace is privileged and its admission policy is the guard. OpenShift labels namespaces from
# the SCCs its accounts hold.
if [ "$PLATFORM" = kubernetes ]; then
    for pair in "$CTL_NS=$CTL_PSA" "$KUEUE_NS=$KUEUE_PSA" "$LOOP_NS=$LOOP_PSA" \
        "$SANDBOX_NS=$SANDBOX_PSA" "$BUILD_NS=privileged"; do
        kc label namespace "${pair%%=*}" pod-security.kubernetes.io/enforce="${pair#*=}" --overwrite >/dev/null
    done
fi

log "deploying the controller, the namespaces and their RBAC"
fill "$ROOT/e2e/kueue/controller.yaml.in" | kc apply -f - >/dev/null
fill "$ROOT/e2e/kueue/cluster.yaml.in" | kc apply -f - >/dev/null
case $PLATFORM in
    openshift) kc label namespace "$WORKLOAD_NS" kueue.openshift.io/managed=true --overwrite >/dev/null ;;
    kubernetes) kc label namespace "$WORKLOAD_NS" pod-security.kubernetes.io/enforce="$WORKLOAD_PSA" --overwrite >/dev/null ;;
esac
for ns in "$CTL_NS" "$LOOP_NS" "$KUEUE_NS" "$SANDBOX_NS" "$WORKLOAD_NS"; do
    kc -n crucible-system get secret "$PULL_SECRET" -o json \
        | jq 'del(.metadata.namespace, .metadata.resourceVersion, .metadata.uid, .metadata.creationTimestamp, .metadata.ownerReferences, .metadata.managedFields)' \
        | kc -n "$ns" apply -f - >/dev/null
done
if [ -n "$ANTHROPIC_KEY_FILE" ]; then
    (umask 077; tr -d '\n' < "$ANTHROPIC_KEY_FILE" > "$WORK/api-key")
    kc -n "$LOOP_NS" create secret generic "$ANTHROPIC_KEY_SECRET" --from-file=api-key="$WORK/api-key" \
        --dry-run=client -o yaml | kc apply -f - >/dev/null
    rm -f "$WORK/api-key"
else
    kc -n crucible-system get secret "$ANTHROPIC_KEY_SECRET" -o json \
        | jq 'del(.metadata.namespace, .metadata.resourceVersion, .metadata.uid, .metadata.creationTimestamp, .metadata.ownerReferences, .metadata.managedFields)' \
        | kc -n "$LOOP_NS" apply -f - >/dev/null
fi
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
if [ "$PLATFORM" = openshift ] && [ -n "${LOOP_SCC_ROLE:-}" ]; then
    kc -n "$LOOP_NS" create rolebinding crucible-kueue-e2e-loop-scc --clusterrole "$LOOP_SCC_ROLE" \
        --serviceaccount "$LOOP_NS:crucible-kueue-e2e-loop" --dry-run=client -o yaml | kc apply -f - >/dev/null
fi
if [ "$PLATFORM" = openshift ] && [ -n "${BUILDER_SCC_ROLE:-${LOOP_SCC_ROLE:-}}" ]; then
    kc -n "$BUILD_NS" create rolebinding crucible-builder-scc --clusterrole "${BUILDER_SCC_ROLE:-$LOOP_SCC_ROLE}" \
        --serviceaccount "$BUILD_NS:default" --dry-run=client -o yaml | kc apply -f - >/dev/null
fi
if [ "$PLATFORM" = openshift ] && [ -n "${SANDBOX_SCC_ROLE:-}" ]; then
    kc -n "$SANDBOX_NS" create rolebinding crucible-sandbox-scc --clusterrole "$SANDBOX_SCC_ROLE" \
        --serviceaccount "$SANDBOX_NS:crucible-sandbox" --dry-run=client -o yaml | kc apply -f - >/dev/null
fi
"$BIN/crucible" deploy render --manifest "$ROOT/e2e/kueue/render.toml" --profile "$WORK/profile.toml" \
    --no-pin 2>/dev/null \
    | yq 'select(.kind != "Pod" and (.kind != "NetworkPolicy" or .metadata.name == "buildit-builders"))' \
    > "$WORK/rendered.yaml"
e2e_scoped "$WORK/rendered.yaml" | kc apply -f - >/dev/null

# The builder admission policy is the guard in BUILD_NS (privileged under Pod Security on
# Kubernetes): a pod buildit would never make must be refused, here a privileged one.
escape=$(jq -cn --arg ns "$BUILD_NS" '{apiVersion: "v1", kind: "Pod",
    metadata: {name: "crucible-kueue-e2e-escape", namespace: $ns, labels: {"buildit.dev/managed-by": "mcp"}},
    spec: {automountServiceAccountToken: false, containers: [{name: "builder",
        image: "quay.io/buildah/stable:latest", securityContext: {privileged: true, runAsUser: 1000}}]}}')
for attempt in $(seq 30); do
    refusal=$(kc apply --dry-run=server -f - <<<"$escape" 2>&1 || true)
    grep -q "ValidatingAdmissionPolicy '$BUILD_NS-buildit-builders'" <<<"$refusal" && break
    [ "$attempt" -lt 30 ] || fail "the builder admission policy admitted a privileged pod in $BUILD_NS: $refusal"
    sleep 2
done
log "the builder admission policy refuses a privileged pod in $BUILD_NS"

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

log "launching $PACK with nonce $NONCE"
cp -R "$PACK" "$WORK/pack"
if [ -d "$PACK/prompts" ]; then
    for prompt in "$PACK"/prompts/*; do
        [ -f "$prompt" ] && fill "$prompt" > "$WORK/pack/prompts/$(basename "$prompt")"
    done
fi
crux draft-create kueue-e2e --description "kueue e2e: $(basename "$PACK")" >/dev/null
crux draft-push kueue-e2e "$WORK/pack" --base-version 1 >/dev/null
crux draft-launch kueue-e2e --max-cost "$MAX_COST" --max-time "$MAX_TIME" \
    ${PARAMS:+--params "$PARAMS"} >/dev/null

# The run's loop pod, once it runs: its name is the run's gateway id.
loop_pod() {
    kc -n "$LOOP_NS" get pods -o json \
        | jq -r '[.items[] | select(.metadata.labels["crucible/run"]) | select(.status.phase == "Running")][0].metadata.name // empty'
}
# The launch's terminal status, or nothing while it runs.
launch_ended() {
    crux playbook-runs --json 2>/dev/null \
        | jq -r '.[0] | select(.status | IN("done", "parked", "failed", "finished", "cancelled")) | "\(.status)\t\(.parked_reason // "")"'
}

if [ "$SMOKE" = 1 ]; then
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
[ "$(jq -r '.metadata.labels["crucible.dev/launched-by"]' <<<"$job")" = "$(label_safe "$LAUNCHER")" ] \
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
POD=$(loop_pod)
[ -n "$POD" ] || fail "no running loop pod in $LOOP_NS"
else
log "waiting for the run's loop pod"
deadline=$((SECONDS + TIMEOUT_SECS))
until POD=$(loop_pod) && [ -n "$POD" ]; do
    ended=$(launch_ended) || ended=
    [ -z "$ended" ] || fail "the run ended (${ended%%$'\t'*}) before its loop pod ran: ${ended#*$'\t'}"
    [ $SECONDS -lt $deadline ] || fail "no loop pod ran within ${TIMEOUT_SECS}s"
    sleep 10
done
RUN=$POD
fi

log "checking the loop pod carries no kueue token"
kueue_env=$(kc -n "$LOOP_NS" get pod "$POD" -o json \
    | jq -r '[.spec.containers[].env[]?.name | select(test("KUEUE"))] | join(",")')
[ -z "$kueue_env" ] || fail "loop pod $POD has kueue env $kueue_env"
servers=$(kc -n "$LOOP_NS" get pod "$POD" -o json \
    | jq -r '[.spec.containers[].env[]? | select(.name == "CRUCIBLE_AGENT_SERVERS") | .value][0] // "[]" | fromjson | [.[].name] | sort | join(",")')
[ "$servers" = buildit,kueue ] || fail "loop pod $POD is granted servers [$servers], not buildit and kueue"
kc -n "$LOOP_NS" get secrets -o json | jq -e '[.items[].data // {} | keys[] | select(test("KUEUE"))] | length == 0' >/dev/null \
    || fail "a Secret in $LOOP_NS carries a kueue key"

log "waiting for the run to finish"
deadline=$((SECONDS + TIMEOUT_SECS))
until ended=$(launch_ended) && [ -n "$ended" ]; do
    [ $SECONDS -lt $deadline ] || fail "the run did not finish within ${TIMEOUT_SECS}s"
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

if [ "$SMOKE" = 1 ]; then
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
else
    jobs=$(kc -n "$WORKLOAD_NS" get jobs -l "crucible.dev/run-id=$RUN" -o name | wc -l | tr -d ' ')
    [ "$jobs" -ge 1 ] || fail "the run of $(basename "$PACK") left no Job in $WORKLOAD_NS (it ended ${ended%%$'\t'*})"
    crux graph "$CRUN" --json > "$WORK/graph.json"
    result=$(jq -r --arg t "$RESULT_TASK" '[.results[]? | select(.task == $t)] | last | .status // empty' "$WORK/graph.json")
    [ "$result" = pass ] \
        || fail "the run of $(basename "$PACK") ended ${ended%%$'\t'*} with $RESULT_TASK ${result:-never run}; see $WORK/graph.json"
    log "the run of $(basename "$PACK") left $jobs Job(s) in $WORKLOAD_NS and $RESULT_TASK passed"
fi

if grep -q CRUCIBLE_KUEUE_TOKEN "$WORK/controller.log" "$WORK/run-log.json"; then
    fail "a static kueue token appears in the run's logs"
fi
log "PASS"
