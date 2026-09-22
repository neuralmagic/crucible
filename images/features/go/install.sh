#!/usr/bin/env bash
set -euo pipefail

arch="$(uname -m)"
case "${arch}" in
    x86_64) goarch=amd64 ;;
    aarch64) goarch=arm64 ;;
    *) echo "unsupported arch ${arch}" >&2; exit 1 ;;
esac
curl -fsSL "https://go.dev/dl/go${PIN_GO}.linux-${goarch}.tar.gz" | tar -C /usr/local -xz
ln -s /usr/local/go/bin/go /usr/local/bin/go
ln -s /usr/local/go/bin/gofmt /usr/local/bin/gofmt
go version
