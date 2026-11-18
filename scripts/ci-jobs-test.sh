#!/usr/bin/env bash
# Tests `cargo xtask ci plan`: each path rule, then the jobs this tree's own paths select.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo test --quiet --locked -p xtask ci::
