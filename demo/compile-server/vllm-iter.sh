#!/bin/sh
# vllm-iter.sh <diff-file> [ninja targets...]
#
# Applies a diff onto the warm vLLM build tree in the vllm-compile pod and
# rebuilds incrementally. With no targets it rebuilds everything changed and
# relinks the extensions; pass an object target for a compile-only check.
# Consecutive calls revert the previous patch before applying the new one.
#
# NAMESPACE is where demo/compile-server/deployment.yaml was applied.
set -e
namespace="${NAMESPACE:-vllm}"
app=vllm-compile
if [ "$#" -lt 1 ]; then
    echo "usage: vllm-iter.sh <diff-file> [ninja targets...]" >&2
    exit 2
fi
pod=$(kubectl get pod -n "$namespace" -l app="$app" -o jsonpath='{.items[0].metadata.name}')
kubectl cp "$1" "$namespace/$pod:/tmp/iter.diff" -c compile
kubectl exec -n "$namespace" "$pod" -c compile -- buildah copy vllmwc /tmp/iter.diff /tmp/iter.diff >/dev/null
shift
targets="$*"
kubectl exec -n "$namespace" "$pod" -c compile -- buildah run vllmwc -- sh -c "
    cd /src/vllm &&
    if [ -f /tmp/last.diff ]; then git apply -R /tmp/last.diff 2>/dev/null || true; fi &&
    git apply /tmp/iter.diff &&
    cp /tmp/iter.diff /tmp/last.diff &&
    b=\$(ls -d build/temp.* | head -1) &&
    t0=\$(date +%s%3N) &&
    if [ -n \"$targets\" ]; then ninja -C \"\$b\" $targets; else ninja -C \"\$b\"; fi; rc=\$?; t1=\$(date +%s%3N) &&
    printf 'built in %sms (rc=%s)\n' \$((t1-t0)) \$rc >&2; exit \$rc"
