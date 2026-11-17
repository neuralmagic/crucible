#!/bin/sh
set -eu
cd "$(dirname "$0")/.."

export SQLX_OFFLINE=true
(cd crucible-controller/ui && bun install --frozen-lockfile && bun --bun run build)
cargo build --release --locked -p crucible-lsp -p crucible -p crucible-controller -p crux \
    --bins --features crucible-controller/embedded-db
