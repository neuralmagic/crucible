#!/usr/bin/env bash
# Build the crucible_kueue Python package for this Linux machine and stage it as
# <dir>/crucible_kueue, ready to copy into a site-packages directory or put on PYTHONPATH.
#   scripts/crucible-kueue-py-stage.sh <dir>
# The extension is abi3 (Python 3.9 and later) and built without libpython, so no interpreter
# is needed here. PROFILE=dev builds unoptimized; CRUCIBLE_KUEUE_PY_TARGET names the cargo target
# directory. On macOS use `maturin develop` in crucible-kueue-py instead.
set -euo pipefail

out=${1:?usage: crucible-kueue-py-stage.sh <dir>}
root=$(cd "$(dirname "$0")/.." && pwd)
crate="$root/crucible-kueue-py"
target=${CRUCIBLE_KUEUE_PY_TARGET:-$crate/target}
[ "$(uname -s)" = Linux ] || { echo "this stages a Linux extension; on $(uname -s) run maturin develop in $crate" >&2; exit 1; }

case "${PROFILE:-release}" in
    release) flags=(--release) built=release ;;
    dev) flags=() built=debug ;;
    *) echo "PROFILE is release or dev, not ${PROFILE}" >&2; exit 1 ;;
esac
PYO3_BUILD_EXTENSION_MODULE=1 cargo build --locked -q ${flags[@]+"${flags[@]}"} \
    --manifest-path "$crate/Cargo.toml" --target-dir "$target"

rm -rf "$out/crucible_kueue"
mkdir -p "$out/crucible_kueue"
cp "$crate"/python/crucible_kueue/*.py "$crate"/python/crucible_kueue/*.pyi \
    "$crate/python/crucible_kueue/py.typed" "$out/crucible_kueue/"
cp "$target/$built/lib_native.so" "$out/crucible_kueue/_native.abi3.so"
