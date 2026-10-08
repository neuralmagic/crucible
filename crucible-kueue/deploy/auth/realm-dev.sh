#!/usr/bin/env bash
# Live changes to realm `crucible` that realm.yaml cannot make once the realm exists. kcadm runs
# inside the Keycloak pod with the bootstrap admin from its own env, so the password never leaves
# the pod. Idempotent.
#
#   realm-dev.sh setup
#       basic scope (sub) on claude-code, the local-dev audience on kueue.read/kueue.submit, and
#       user attributes editable by admins only (a person must not set their own queues).
#   realm-dev.sh grant <username> <namespace> <viewer|submitter> [queue...]
#       tenant scope for a person who has logged in once through OpenShift.
set -euo pipefail
CTX=${CTX:-crucible-system/api-openshift-nm-rdu4-corp-redhat-com:6443/wseaton}
REALM=crucible
LOCAL_AUDIENCE=${LOCAL_AUDIENCE:-http://localhost:8850/mcp}

kc() {
  oc --context "$CTX" -n rhbk exec -i keycloak-0 -- /bin/bash -c '
    K=/opt/keycloak/bin/kcadm.sh
    $K config credentials --config /tmp/kcadm.config --server http://localhost:8080 --realm master \
      --user "$KC_BOOTSTRAP_ADMIN_USERNAME" --password "$KC_BOOTSTRAP_ADMIN_PASSWORD" >/dev/null
    exec $K "$@" --config /tmp/kcadm.config' kc "$@"
}

id_of() { # <collection> <field> <value>
  kc get "$1" -r "$REALM" --fields id,"$2" | jq -r --arg v "$3" --arg f "$2" '.[] | select(.[$f] == $v) | .id'
}

mapper_names() { # <client-scope id>
  kc get "client-scopes/$1/protocol-mappers/models" -r "$REALM" | jq -r '.[].name'
}

setup() {
  local basic client scope
  basic=$(id_of client-scopes name basic)
  if [ -z "$basic" ]; then
    kc create client-scopes -r "$REALM" -s name=basic -s protocol=openid-connect \
      -s 'attributes."include.in.token.scope"=false' -s 'attributes."display.on.consent.screen"=false'
    basic=$(id_of client-scopes name basic)
  fi
  if ! mapper_names "$basic" | grep -qx sub; then
    kc create "client-scopes/$basic/protocol-mappers/models" -r "$REALM" -s name=sub \
      -s protocol=openid-connect -s protocolMapper=oidc-sub-mapper \
      -s 'config."access.token.claim"=true' -s 'config."introspection.token.claim"=true'
  fi
  client=$(id_of clients clientId claude-code)
  kc update "clients/$client/default-client-scopes/$basic" -r "$REALM"

  for name in kueue.read kueue.submit; do
    scope=$(id_of client-scopes name "$name")
    if ! mapper_names "$scope" | grep -qx aud-kueue-local; then
      kc create "client-scopes/$scope/protocol-mappers/models" -r "$REALM" -s name=aud-kueue-local \
        -s protocol=openid-connect -s protocolMapper=oidc-audience-mapper \
        -s "config.\"included.custom.audience\"=$LOCAL_AUDIENCE" \
        -s 'config."access.token.claim"=true' -s 'config."id.token.claim"=false'
    fi
  done

  kc get users/profile -r "$REALM" | jq '.unmanagedAttributePolicy = "ADMIN_EDIT"' |
    kc update users/profile -r "$REALM" -f -
  echo "realm $REALM ready for claude-code"
}

grant() {
  local username=$1 namespace=$2 role=$3
  shift 3
  local user queues
  user=$(kc get users -r "$REALM" -q "username=$username" -q exact=true --fields id | jq -r '.[0].id // empty')
  if [ -z "$user" ]; then
    echo "no user $username in realm $REALM; log in once through OpenShift first" >&2
    exit 1
  fi
  queues=$(printf '%s\n' "$@" | jq -R . | jq -cs 'map(select(length > 0))')
  kc get "users/$user" -r "$REALM" |
    jq --arg ns "$namespace" --arg role "$role" --argjson q "$queues" \
      '.attributes = ((.attributes // {}) + {crucible_namespace: [$ns], crucible_role: [$role], crucible_queues: $q})' |
    kc update "users/$user" -r "$REALM" -f -
  kc get "users/$user" -r "$REALM" | jq -c '{username, attributes}'
}

case "${1:-}" in
  setup) setup ;;
  grant) shift; grant "$@" ;;
  *) sed -n '2,12p' "$0"; exit 2 ;;
esac
