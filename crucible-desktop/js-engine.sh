#!/bin/sh
# Fetch the engine JavaScript view plugins run in, which the app is built with: Javy's QuickJS
# module at a pinned release, checked against its digest. Does nothing when it is already here.
set -eu
cd "$(dirname "$0")"

RELEASE=v9.1.0
DIGEST=180230f9346dc4b7d7139791280c9f4da09b2292eef751a3d35ae80154d88350
module=assets/js-engine/plugin.wasm

digest() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

if [ -f "$module" ] && [ "$(digest "$module")" = "$DIGEST" ]; then
    exit 0
fi
mkdir -p "$(dirname "$module")"
curl -fsSL --retry 3 "https://github.com/bytecodealliance/javy/releases/download/$RELEASE/plugin.wasm.gz" \
    | gunzip > "$module.part"
if [ "$(digest "$module.part")" != "$DIGEST" ]; then
    rm -f "$module.part"
    echo "js-engine: the fetched module is not the one pinned" >&2
    exit 1
fi
mv "$module.part" "$module"
