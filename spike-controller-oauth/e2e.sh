#!/usr/bin/env bash
# Controller-as-issuer spike, end to end on this machine against pirate's RBAC.
#
#   Keycloak (podman, :8180)    stand-in for Red Hat SSO: login `weaton`, email $EMAIL
#   crucible-controller (:8890) native login against it; issues people's tokens
#   crucible-kueue (:8851)      --person-issuer = the controller, SAR on pirate (identity = "link")
#   crucible-kueue (:8852)      the same on waldorf (identity = { claim = "email" })
#
# Then, in a terminal:
#   claude mcp add --transport http kueue-spike http://localhost:8851/mcp \
#     --client-id claude-code --callback-port 8765
#   claude mcp add --transport http kueue-waldorf http://localhost:8852/mcp \
#     --client-id claude-code --callback-port 8765
#   claude, /mcp, authenticate both. pirate needs a link first, offered on the consent page (and on
#   Settings -> Cluster identities): OpenShift sign-in when PIRATE_LINK_SECRET is set, else paste
#   `oc whoami -t`. waldorf needs nothing: its username is the SSO email.
#
# PIRATE_LINK_SECRET=<file> turns on the OpenShift sign-in for pirate. It must hold the secret of
# an OAuthClient on pirate rendered for this controller:
#   crucible deploy render --profile $WORK/profile.toml --cluster-link pirate
# (cluster-scoped; creating it needs cluster-admin on pirate).
#
#   CONTEXT=<pirate context> WALDORF_CONTEXT=<waldorf context> spike-controller-oauth/e2e.sh up|down
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=${WORK:-$ROOT/spike-controller-oauth/.work}
CONTEXT=${CONTEXT:-crucible-system/api-openshift-nm-rdu4-corp-redhat-com:6443/wseaton}
WALDORF_CONTEXT=${WALDORF_CONTEXT:-coreweave-waldorf}
EMAIL=${EMAIL:-weaton@redhat.com}
BIN=$(cargo metadata -q --manifest-path "$ROOT/Cargo.toml" --format-version 1 --no-deps | jq -r .target_directory)/debug
CONTROLLER=http://localhost:8890
KUEUE=http://localhost:8851/mcp
KUEUE_WALDORF=http://localhost:8852/mcp
KEYCLOAK=http://localhost:8180
log() { printf '[spike] %s\n' "$*" >&2; }

down() {
    [ -f "$WORK/controller.pid" ] && kill "$(cat "$WORK/controller.pid")" 2>/dev/null || true
    [ -f "$WORK/kueue.pid" ] && kill "$(cat "$WORK/kueue.pid")" 2>/dev/null || true
    [ -f "$WORK/kueue-waldorf.pid" ] && kill "$(cat "$WORK/kueue-waldorf.pid")" 2>/dev/null || true
    podman rm -f crucible-spike-keycloak >/dev/null 2>&1 || true
    rm -f "$WORK"/*.pid
}

up() {
    mkdir -p "$WORK/clusters/pirate" "$WORK/clusters/waldorf" "$WORK/state" "$WORK/realm" "$WORK/links"
    (umask 077
     [ -s "$WORK/weaton.password" ] || openssl rand -hex 12 > "$WORK/weaton.password"
     [ -s "$WORK/signing.pem" ] || openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$WORK/signing.pem"
     [ -s "$WORK/state/webhook.key" ] || head -c 32 /dev/urandom | base64 > "$WORK/state/webhook.key"
     kubectl config view --raw --minify --flatten --context "$CONTEXT" > "$WORK/clusters/pirate/kubeconfig"
     kubectl config view --raw --minify --flatten --context "$WALDORF_CONTEXT" > "$WORK/clusters/waldorf/kubeconfig"
     rm -f "$WORK/links/pirate"
     [ -z "${PIRATE_LINK_SECRET:-}" ] || cp "$PIRATE_LINK_SECRET" "$WORK/links/pirate")

    jq --arg pw "$(cat "$WORK/weaton.password")" --arg email "$EMAIL" '.users += [{
          username: "weaton", enabled: true, emailVerified: true, email: $email,
          firstName: "Will", lastName: "Eaton", requiredActions: [],
          credentials: [{type: "password", value: $pw, temporary: false}],
          groups: ["/platform-devs"], realmRoles: ["offline_access"]}]' \
        "$ROOT/crucible-controller/tests/keycloak/crucible-realm.json" > "$WORK/realm/crucible-realm.json"

    cat > "$WORK/profile.toml" <<EOF
[cluster]
loop_namespace    = "crucible-system"
rig_namespace     = "crucible-system"
sandbox_namespace = "crucible-sandboxes"
service_account   = "crucible-crucible-controller"
supervisor_image  = "ghcr.io/neuralmagic/openshell-supervisor:latest"

[image]
loop        = "quay.io/wseaton/crucible-loop:latest"
pull_secret = "quay-wseaton-pull"

[clusters.pirate]
kubeconfig_secret = "pirate-kubeconfig"
identity          = "link"

[clusters.waldorf]
kubeconfig_secret = "waldorf-kubeconfig"
identity          = { claim = "email" }

[cluster_link]
external_url = "$CONTROLLER"
openshift    = ["pirate"]
EOF

    log "starting keycloak on $KEYCLOAK"
    podman rm -f crucible-spike-keycloak >/dev/null 2>&1 || true
    podman run -d --name crucible-spike-keycloak -p 8180:8080 \
        -e KC_BOOTSTRAP_ADMIN_USERNAME=admin -e "KC_BOOTSTRAP_ADMIN_PASSWORD=$(openssl rand -hex 12)" \
        -v "$WORK/realm:/opt/keycloak/data/import:ro" \
        quay.io/keycloak/keycloak:26.4 start-dev --import-realm >/dev/null
    for _ in $(seq 90); do
        curl -fsS "$KEYCLOAK/realms/crucible/.well-known/openid-configuration" >/dev/null 2>&1 && break
        sleep 2
    done
    curl -fsS "$KEYCLOAK/realms/crucible/.well-known/openid-configuration" >/dev/null

    log "starting the controller on $CONTROLLER"
    env -u CONTROLLER_API_TOKEN -u CONTROLLER_PROXY_TOKEN -u VAULT_ADDR \
        KUBECONFIG=/dev/null \
        DATABASE_URL=embedded \
        POD_NAMESPACE=crucible-controller-spike \
        CONTROLLER_API_ADDR=127.0.0.1:8890 \
        CONTROLLER_PUBLIC_URL="$CONTROLLER" \
        CONTROLLER_WEBHOOK_KEY_FILE="$WORK/state/webhook.key" \
        CONTROLLER_AUTH_MODE=native \
        CONTROLLER_OIDC_ISSUER="$KEYCLOAK/realms/crucible" \
        CONTROLLER_OIDC_CLIENT_ID=crucible-controller \
        CONTROLLER_OIDC_CLIENT_SECRET="$(jq -r '.clients[] | select(.clientId=="crucible-controller") | .secret' "$WORK/realm/crucible-realm.json")" \
        CONTROLLER_OIDC_REDIRECT_URL="$CONTROLLER/auth/callback" \
        CONTROLLER_SESSION_SECURE=false \
        CONTROLLER_ADMINS=weaton \
        CONTROLLER_AGENT_ISSUER="$CONTROLLER" \
        CONTROLLER_AGENT_SIGNING_KEY_FILE="$WORK/signing.pem" \
        CONTROLLER_AGENT_SVID_ISSUER=https://oidc-discovery.invalid \
        CONTROLLER_AGENT_SVID_JWKS_URI=https://oidc-discovery.invalid/keys \
        CONTROLLER_AGENT_SANDBOX_ID_PREFIX=spiffe://pirate.crucible/openshell/sandbox/ \
        CONTROLLER_AGENT_AUDIENCES="$KUEUE=kueue.read kueue.submit" \
        CONTROLLER_PEOPLE_AUDIENCES="pirate:$KUEUE=kueue.read kueue.submit,waldorf:$KUEUE_WALDORF=kueue.read kueue.submit" \
        CONTROLLER_CLUSTER_LINK_SECRETS="$WORK/links" \
        CONTROLLER_DEPLOY_PROFILE="$WORK/profile.toml" \
        CONTROLLER_CLUSTERS_DIR="$WORK/clusters" \
        CONTROLLER_PLAYBOOK_EXECUTOR=local \
        CONTROLLER_SCOPE_EXECUTOR=disabled \
        CONTROLLER_STATE_DIR="$WORK/state" \
        CRUCIBLE_BIN="$BIN/crucible" \
        RUST_LOG=info,crucible_controller::agent_auth=debug \
        "$BIN/crucible-controller" autopilot > "$WORK/controller.log" 2>&1 &
    echo $! > "$WORK/controller.pid"
    for _ in $(seq 60); do
        curl -fsS "$CONTROLLER/.well-known/oauth-authorization-server" >/dev/null 2>&1 && break
        sleep 2
    done
    curl -fsS "$CONTROLLER/.well-known/oauth-authorization-server" | jq .

    log "starting crucible-kueue on $KUEUE"
    RUST_LOG=info,crucible_mcp_auth=debug "$BIN/crucible-kueue" \
        --bind 127.0.0.1:8851 \
        --context "$CONTEXT" \
        --allowed-host localhost:8851 \
        --person-issuer "$CONTROLLER=$CONTROLLER/.well-known/jwks.json" \
        --resource "$KUEUE" \
        --tool-policy "$ROOT/crucible-kueue/deploy/kueue-policy.toml" \
        --kube-authz openshift > "$WORK/kueue.log" 2>&1 &
    echo $! > "$WORK/kueue.pid"
    for _ in $(seq 30); do
        curl -fsS "http://localhost:8851/.well-known/oauth-protected-resource/mcp" >/dev/null 2>&1 && break
        sleep 1
    done
    curl -fsS "http://localhost:8851/.well-known/oauth-protected-resource/mcp" | jq .

    log "starting crucible-kueue on $KUEUE_WALDORF"
    RUST_LOG=info,crucible_mcp_auth=debug "$BIN/crucible-kueue" \
        --bind 127.0.0.1:8852 \
        --context "$WALDORF_CONTEXT" \
        --allowed-host localhost:8852 \
        --person-issuer "$CONTROLLER=$CONTROLLER/.well-known/jwks.json" \
        --resource "$KUEUE_WALDORF" \
        --tool-policy "$ROOT/crucible-kueue/deploy/kueue-policy.toml" \
        --kube-authz token > "$WORK/kueue-waldorf.log" 2>&1 &
    echo $! > "$WORK/kueue-waldorf.pid"
    for _ in $(seq 30); do
        curl -fsS "http://localhost:8852/.well-known/oauth-protected-resource/mcp" >/dev/null 2>&1 && break
        sleep 1
    done
    curl -fsS "http://localhost:8852/.well-known/oauth-protected-resource/mcp" | jq .
    log "keycloak login: weaton / password in $WORK/weaton.password"
}

case "${1:-up}" in
    up) up ;;
    down) down ;;
    *) echo "usage: $0 up|down" >&2; exit 2 ;;
esac
