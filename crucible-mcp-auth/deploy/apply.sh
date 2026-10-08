#!/usr/bin/env bash
# Apply the MCP auth stack to pirate in order. Safe to re-run: existing Secrets are kept, and
# the OAuthClient secret is read back from rhbk/keycloak-openshift-oauth so both sides match.
set -euo pipefail
cd "$(dirname "$0")"
CTX=${CTX:-crucible-system/api-openshift-nm-rdu4-corp-redhat-com:6443/wseaton}
oc() { command oc --context "$CTX" "$@"; }

oc apply -f spire.yaml -f clusterspiffeid.yaml

oc -n rhbk get secret keycloak-db >/dev/null 2>&1 ||
  oc -n rhbk create secret generic keycloak-db \
    --from-literal=username=keycloak --from-literal=password="$(openssl rand -hex 24)"

oc -n rhbk get secret keycloak-openshift-oauth >/dev/null 2>&1 ||
  oc -n rhbk create secret generic keycloak-openshift-oauth \
    --from-literal=client-secret="$(openssl rand -hex 32)"
OAUTH_SECRET=$(oc -n rhbk get secret keycloak-openshift-oauth -o jsonpath='{.data.client-secret}' | base64 -d)
sed "s|\${OPENSHIFT_OAUTH_CLIENT_SECRET}|$OAUTH_SECRET|" openshift-oauth-client.yaml | oc apply -f -

oc apply -f keycloak.yaml
oc -n rhbk wait keycloak/keycloak --for=condition=Ready --timeout=10m
oc apply -f realm.yaml
