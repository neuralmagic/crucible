#!/usr/bin/env bash
set -euo pipefail

npm install -g "opencode-ai@${PIN_OPENCODE}"

# npm links /usr/local/bin/opencode to a JS launcher that execs the platform binary. The sandbox
# egress policy matches the executable that opens the socket, and the harness allowlists
# /usr/local/bin/opencode, so that path has to resolve to the binary itself.
case "$(uname -m)" in
    x86_64) platform=linux-x64 ;;
    aarch64) platform=linux-arm64 ;;
    *) echo "unsupported arch $(uname -m)" >&2; exit 1 ;;
esac
# The platform package is an optional dependency of opencode-ai, so npm nests it under the
# package rather than hoisting it.
native="$(find "$(npm root -g)" -path "*/opencode-${platform}/bin/opencode" -type f | head -n 1)"
[ -n "${native}" ] && [ -x "${native}" ] || { echo "no native opencode binary for ${platform} under $(npm root -g)" >&2; exit 1; }
ln -sf "${native}" /usr/local/bin/opencode
opencode --version
