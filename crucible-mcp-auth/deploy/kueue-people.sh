#!/usr/bin/env bash
# Deploy the person-facing crucible-kueue (kueue-people.yaml) at IMAGE, with the shipped tool
# policy, and grant its service account the workload verbs in each of PEOPLE_NAMESPACES.
#
#   IMAGE=quay.io/wseaton/crucible-kueue@sha256:... deploy/kueue-people.sh
set -euo pipefail
cd "$(dirname "$0")"
CTX=${CTX:-crucible-system/api-openshift-nm-rdu4-corp-redhat-com:6443/wseaton}
: "${IMAGE:?set IMAGE to a digest-pinned crucible-kueue image}"
PEOPLE_NAMESPACES=${PEOPLE_NAMESPACES:-crucible-kueue-e2e}
oc() { command oc --context "$CTX" "$@"; }

oc -n crucible-system create configmap crucible-kueue-people-policy \
  --from-file=kueue-policy.toml=../../crucible-kueue/deploy/kueue-policy.toml \
  --dry-run=client -o yaml | oc apply -f -
IMAGE="$IMAGE" envsubst '${IMAGE}' < kueue-people.yaml | oc apply -f -
for ns in $PEOPLE_NAMESPACES; do
  oc -n "$ns" create rolebinding crucible-kueue-people-workloads \
    --clusterrole=crucible-kueue-workloads --serviceaccount=crucible-system:crucible-kueue-people \
    --dry-run=client -o yaml | oc apply -f -
done
oc -n crucible-system rollout restart deploy/crucible-kueue-people
oc -n crucible-system rollout status deploy/crucible-kueue-people --timeout=120s
