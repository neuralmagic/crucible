#!/usr/bin/env bash
set -euo pipefail

npm install -g "@openai/codex@${PIN_CODEX}"

# npm links /usr/local/bin/codex to a node shim that execs the native binary from the
# per-platform package. The sandbox egress policy matches the executable that opens the
# socket, and the harness allowlists /usr/local/bin/codex, so that path has to resolve to
# the binary itself.
case "$(uname -m)" in
    x86_64) target=x86_64 npm_arch=x64 ;;
    aarch64) target=aarch64 npm_arch=arm64 ;;
    *) echo "unsupported arch $(uname -m)" >&2; exit 1 ;;
esac
native="$(npm root -g)/@openai/codex/node_modules/@openai/codex-linux-${npm_arch}/vendor/${target}-unknown-linux-musl/bin/codex"
[ -x "${native}" ] || { echo "no native codex binary at ${native}" >&2; exit 1; }
ln -sf "${native}" /usr/local/bin/codex
codex --version
