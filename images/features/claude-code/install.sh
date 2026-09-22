#!/usr/bin/env bash
set -euo pipefail

# On a base without node (custom, non-UBI bases), pull the pinned dist tarball.
if ! command -v npm >/dev/null; then
    arch="$(uname -m)"
    case "${arch}" in
        x86_64) narch=x64 ;;
        aarch64) narch=arm64 ;;
        *) echo "unsupported arch ${arch}" >&2; exit 1 ;;
    esac
    curl -fsSL "https://nodejs.org/dist/v${PIN_NODE}/node-v${PIN_NODE}-linux-${narch}.tar.gz" \
        | tar -xz -C /usr/local --strip-components=1
fi

npm install -g "@anthropic-ai/claude-code@${PIN_CLAUDE_CODE}"
claude --version
