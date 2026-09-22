#!/usr/bin/env bash
set -euo pipefail

export CARGO_HOME=/usr/local/cargo
curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path \
    --profile default --default-toolchain "${PIN_RUST}"
chmod -R a+rX /usr/local/rustup /usr/local/cargo
cargo --version
cargo clippy --version
