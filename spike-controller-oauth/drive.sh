#!/usr/bin/env bash
# Drive one MCP client login against the e2e stack (e2e.sh up) without a person at the keyboard:
# authorization request with PKCE, Keycloak sign-in, the consent page's API, a cluster link when the
# cluster needs one, the code exchange, then MCP calls to crucible-kueue with the token.
#
#   spike-controller-oauth/drive.sh <resource> [namespace] [link-context]
#
# <link-context>: when the resource's cluster is `link` and unlinked, link it with
# `oc whoami -t` from this kube context. Prints the token's claims and each tool's answer; writes the
# access token to $WORK/token-<host:port>.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=${WORK:-$ROOT/spike-controller-oauth/.work}
CONTROLLER=${CONTROLLER:-http://localhost:8890}
RESOURCE=$1
NAMESPACE=${2:-crucible-system}
LINK_CONTEXT=${3:-}
REDIRECT=http://127.0.0.1:8765/callback
JAR=$(mktemp)
trap 'rm -f "$JAR"' EXIT
log() { printf '[drive] %s\n' "$*" >&2; }
b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
api() { curl -fsS -b "$JAR" -c "$JAR" -H 'Sec-Fetch-Site: same-origin' "$@"; }

verifier=$(openssl rand -hex 32)
challenge=$(printf %s "$verifier" | openssl dgst -binary -sha256 | b64url)
state=$(openssl rand -hex 8)
authorize="$CONTROLLER/oauth/authorize?response_type=code&client_id=claude-code&code_challenge_method=S256\
&code_challenge=$challenge&state=$state&scope=kueue.read&redirect_uri=$(jq -rn --arg v "$REDIRECT" '$v|@uri')\
&resource=$(jq -rn --arg v "$RESOURCE" '$v|@uri')"

log "authorization request for $RESOURCE"
login_page=$(curl -fsS -L -b "$JAR" -c "$JAR" "$authorize")
action=$(printf %s "$login_page" | tr '\n' ' ' | sed -E 's/.*id="kc-form-login"[^>]*action="([^"]*)".*/\1/' | sed 's/&amp;/\&/g')
landed=$(curl -sS -L -b "$JAR" -c "$JAR" -o /dev/null -w '%{url_effective}' \
    --data-urlencode username=weaton --data-urlencode "password=$(cat "$WORK/weaton.password")" "$action")
id=${landed##*consent?id=}
[ "$id" != "$landed" ] || { log "did not reach the consent page: $landed"; exit 1; }
log "consent page: $landed"

consent=$(api "$CONTROLLER/api/oauth/consent?id=$id")
printf '%s\n' "$consent" | jq '{client_id, resource, scopes, login, identity}'
cluster=$(jq -r .identity.cluster <<<"$consent")
if [ "$(jq -r .identity.kube_user <<<"$consent")" = null ] && [ -n "$LINK_CONTEXT" ]; then
    log "linking $cluster with a token from $LINK_CONTEXT"
    jq -n --arg t "$(oc --context "$LINK_CONTEXT" whoami -t)" '{cluster_token: $t}' |
        api -X PUT -H 'Content-Type: application/json' --data @- \
            "$CONTROLLER/api/me/cluster-identities/$cluster" | jq '{cluster, kube_user, source}'
fi

decision=$(api -X POST -H 'Content-Type: application/json' \
    --data "$(jq -n --arg id "$id" '{id: $id, decision: "approve"}')" "$CONTROLLER/api/oauth/consent")
redirect=$(jq -r .redirect_url <<<"$decision")
code=$(printf %s "$redirect" | sed -E 's/.*[?&]code=([^&]*).*/\1/')
[ "$(printf %s "$redirect" | sed -E 's/.*[?&]state=([^&]*).*/\1/')" = "$state" ] || { log "state lost"; exit 1; }
log "approved; redirect ${redirect%%\?*}"

token=$(curl -fsS "$CONTROLLER/oauth/token" -d grant_type=authorization_code -d client_id=claude-code \
    --data-urlencode "code=$code" --data-urlencode "redirect_uri=$REDIRECT" \
    --data-urlencode "code_verifier=$verifier" --data-urlencode "resource=$RESOURCE" | jq -r .access_token)
host=$(printf %s "$RESOURCE" | sed -E 's#^https?://([^/]*)/.*#\1#')
printf %s "$token" > "$WORK/token-$host"
claims() {
    local p
    p=$(printf %s "$1" | cut -d. -f2 | tr '_-' '/+')
    while [ $((${#p} % 4)) -ne 0 ]; do p="$p="; done
    printf %s "$p" | openssl base64 -d -A
}
claims "$token" | jq '{sub, aud, azp, scope, kube_user, kube_cluster}'

mcp() {
    curl -sS "$RESOURCE" -H "Authorization: Bearer $token" -H 'Content-Type: application/json' \
        -H 'Accept: application/json, text/event-stream' -H 'MCP-Protocol-Version: 2025-06-18' "$@"
}
mcp -o /dev/null --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"drive","version":"0"}}}'
for call in "whoami {\"namespace\":\"$NAMESPACE\"}" "queues_list {\"namespace\":\"$NAMESPACE\"}"; do
    tool=${call%% *}
    log "tools/call $tool"
    mcp --data "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"$tool\",\"arguments\":${call#* }}}" |
        sed -n 's/^data: //p; /^{/p' | jq -r '.result.content[0].text // .error.message // .' | head -20
done
