#!/usr/bin/env bash
set -euo pipefail

arch="$(uname -m)"
case "${arch}" in
    x86_64 | aarch64) ;;
    *) echo "unsupported arch ${arch}" >&2; exit 1 ;;
esac
name="nu-${PIN_NU}-${arch}-unknown-linux-musl"
curl -fsSL -o /tmp/nu.tar.gz \
    "https://github.com/nushell/nushell/releases/download/${PIN_NU}/${name}.tar.gz"
tar -xzf /tmp/nu.tar.gz -C /tmp
cp "/tmp/${name}/nu" /usr/local/bin/nu
rm -rf /tmp/nu.tar.gz "/tmp/${name}"
nu --version
