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
#   REPLICAS=2 ACCESS_TTL=60 spike-controller-oauth/e2e.sh up      # two controllers, one database
#   REPLICAS=2 ACCESS_TTL=60 spike-controller-oauth/e2e.sh restart-controllers
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=${WORK:-$ROOT/spike-controller-oauth/.work}
CONTEXT=${CONTEXT:-crucible-system/api-openshift-nm-rdu4-corp-redhat-com:6443/wseaton}
WALDORF_CONTEXT=${WALDORF_CONTEXT:-coreweave-waldorf}
EMAIL=${EMAIL:-weaton@redhat.com}
REPLICAS=${REPLICAS:-1}
PEOPLE_DB=${PEOPLE_DB:-postgres://postgres:ci@localhost:55433/crucible_people_e2e}
ACCESS_TTL=${ACCESS_TTL:-900}
BIN=$(cargo metadata -q --manifest-path "$ROOT/Cargo.toml" --format-version 1 --no-deps | jq -r .target_directory)/debug
CONTROLLER=${CONTROLLER:-http://localhost:8890}
CONTROLLER_PORT=${CONTROLLER##*:}
KUEUE=http://localhost:8851/mcp
KUEUE_WALDORF=http://localhost:8852/mcp
KEYCLOAK=http://localhost:8180
log() { printf '[spike] %s\n' "$*" >&2; }

# One controller: <name> <port> <database url>. Every replica shares the signing key, the issuer
# URL ($CONTROLLER) and the database.
start_controller() {
    local name=$1 port=$2 db=$3
    mkdir -p "$WORK/state-$name"
    log "starting controller $name on :$port"
    env -u CONTROLLER_API_TOKEN -u CONTROLLER_PROXY_TOKEN -u VAULT_ADDR \
        KUBECONFIG=/dev/null \
        DATABASE_URL="$db" \
        POD_NAMESPACE=crucible-controller-spike \
        CONTROLLER_API_ADDR="127.0.0.1:$port" \
        CONTROLLER_PUBLIC_URL="$CONTROLLER" \
        CONTROLLER_WEBHOOK_KEY_FILE="$WORK/webhook.key" \
        CONTROLLER_AUTH_MODE=native \
        CONTROLLER_OIDC_ISSUER="$KEYCLOAK/realms/crucible" \
        CONTROLLER_OIDC_CLIENT_ID=crucible-controller \
        CONTROLLER_OIDC_CLIENT_SECRET="$(jq -r '.clients[] | select(.clientId=="crucible-controller") | .secret' "$WORK/realm/crucible-realm.json")" \
        CONTROLLER_OIDC_REDIRECT_URL="$CONTROLLER/auth/callback" \
        CONTROLLER_SESSION_SECURE=false \
        CONTROLLER_ADMINS=weaton \
        CONTROLLER_AGENT_ISSUER="$CONTROLLER" \
        CONTROLLER_AGENT_SIGNING_KEY_FILE="$WORK/signing.pem" \
        CONTROLLER_AGENT_SPIRE="pirate=https://oidc-discovery.invalid spiffe://pirate.crucible/openshell/gateway/ spire/oidc:443" \
        CONTROLLER_AGENT_AUDIENCES="$KUEUE=kueue.read kueue.submit" \
        CONTROLLER_PEOPLE_AUDIENCES="pirate:$KUEUE=kueue.read kueue.submit,waldorf:$KUEUE_WALDORF=kueue.read kueue.submit" \
        CONTROLLER_CLUSTER_LINK_SECRETS="$WORK/links" \
        CONTROLLER_DEPLOY_PROFILE="$WORK/profile.toml" \
        CONTROLLER_CLUSTERS_DIR="$WORK/clusters" \
        CONTROLLER_UI_DIR="$ROOT/crucible-controller/ui/dist" \
        CONTROLLER_PLAYBOOK_EXECUTOR=local \
        CONTROLLER_SCOPE_EXECUTOR=disabled \
        CONTROLLER_STATE_DIR="$WORK/state-$name" \
        CONTROLLER_PEOPLE_ACCESS_TTL_SECS="$ACCESS_TTL" \
        CRUCIBLE_BIN="$BIN/crucible" \
        RUST_LOG=info,crucible_controller::agent_auth=debug \
        "$BIN/crucible-controller" autopilot >> "$WORK/controller-$name.log" 2>&1 &
    echo $! > "$WORK/controller-$name.pid"
    for _ in $(seq 60); do
        curl -fsS "http://127.0.0.1:$port/.well-known/oauth-authorization-server" >/dev/null 2>&1 && break
        sleep 2
    done
    curl -fsS "http://127.0.0.1:$port/.well-known/oauth-authorization-server" >/dev/null

}

# REPLICAS=1: one controller at $CONTROLLER with its own embedded Postgres. REPLICAS=2: two on :8891
# and :8892 sharing $PEOPLE_DB, behind caddy round-robin at $CONTROLLER.
start_controllers() {
    if [ "$REPLICAS" = 1 ]; then
        start_controller a "$CONTROLLER_PORT" embedded
    else
        start_controller a 8891 "$PEOPLE_DB"
        start_controller b 8892 "$PEOPLE_DB"
        if [ ! -f "$WORK/caddy.pid" ] || ! kill -0 "$(cat "$WORK/caddy.pid")" 2>/dev/null; then
            cat > "$WORK/Caddyfile" <<CADDY
{
    admin off
    auto_https off
}
:$CONTROLLER_PORT {
    reverse_proxy 127.0.0.1:8891 127.0.0.1:8892 {
        lb_policy round_robin
    }
}
:8851 {
    bind ::1
    reverse_proxy 127.0.0.1:8851
}
:8852 {
    bind ::1
    reverse_proxy 127.0.0.1:8852
}
CADDY
            caddy run --config "$WORK/Caddyfile" --adapter caddyfile > "$WORK/caddy.log" 2>&1 &
            echo $! > "$WORK/caddy.pid"
        fi
    fi
    for _ in $(seq 30); do
        curl -fsS "$CONTROLLER/.well-known/oauth-authorization-server" >/dev/null 2>&1 && break
        sleep 1
    done
    curl -fsS "$CONTROLLER/.well-known/oauth-authorization-server" | jq -c '{issuer, token_endpoint}'
}

stop_controllers() {
    for pid in "$WORK"/controller-*.pid; do
        [ -f "$pid" ] && kill "$(cat "$pid")" 2>/dev/null || true
        rm -f "$pid"
    done
    sleep 2
}

down() {
    stop_controllers
    [ -f "$WORK/caddy.pid" ] && kill "$(cat "$WORK/caddy.pid")" 2>/dev/null || true
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
     [ -s "$WORK/webhook.key" ] || head -c 32 /dev/urandom | base64 > "$WORK/webhook.key"
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
sandbox_driver    = "kubernetes"
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

    start_controllers

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
    restart-controllers) stop_controllers; start_controllers ;;
    *) echo "usage: $0 up|down|restart-controllers" >&2; exit 2 ;;
esac
