#!/usr/bin/env bash
set -euo pipefail

arch="$(uname -m)"
case "${arch}" in
    x86_64) karch=amd64 ;;
    aarch64) karch=arm64 ;;
    *) echo "unsupported arch ${arch}" >&2; exit 1 ;;
esac
curl -fsSL -o /usr/local/bin/kubectl \
    "https://dl.k8s.io/release/v${PIN_KUBECTL}/bin/linux/${karch}/kubectl"
chmod +x /usr/local/bin/kubectl
kubectl version --client
