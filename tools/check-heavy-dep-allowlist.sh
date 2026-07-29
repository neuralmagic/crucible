#!/usr/bin/env bash
# Heavy-dep allow-list guard: a crate may only link one of the "heavy" dependency clusters
# below if it's on that dep's allow-list. This is a deny-by-default guardrail against a heavy
# dep quietly leaking into a crate that shouldn't need it (e.g. k8s-openapi/kube ending up in
# crucible-contract, which must stay a lightweight leaf every other crate rebuilds constantly
# against).
#
# Snapshot taken 2026-07-10 against origin/main. The allow-lists below are the CURRENT state,
# not the aspirational end-state from the workspace refactor plan. If you add a heavy dep to a
# crate not already on its allow-list, either don't, or add it here in the same PR and say why
# in the PR description.
set -euo pipefail

# dep-name:space-separated-list-of-crate-dirs-allowed-to-depend-on-it-directly. Plain array (not
# an associative array) so this runs under both GNU bash on CI and macOS's ancient system bash.
ALLOW=(
  "k8s-openapi:crucible crucible-controller forge"
  "kube:crucible-controller forge"
  "sqlx:crucible-controller"
  "git2:crucible crucible-vcs forge epp-tools"
  "aws-sdk-s3:crucible"
  "ratatui:"
  "crossterm:"
  "parquet:crucible-controller"
)

# Crate dirs that hold an actual [package] (skip the virtual workspace root Cargo.toml, whose
# [workspace.dependencies] table pins shared versions but isn't itself a dependency edge).
CRATE_MANIFESTS=$(find . -name Cargo.toml -not -path '*/target/*' \
  | sort \
  | while read -r f; do grep -q '^\[package\]' "$f" && echo "$f"; done)

fail=0
for manifest in $CRATE_MANIFESTS; do
  crate_dir=$(dirname "$manifest" | sed -E 's#^\./##')
  crate_name=$(basename "$crate_dir")

  # Slice out only the [dependencies]/[dev-dependencies]/[build-dependencies]/
  # [target.*.dependencies] tables — skip everything else ([package], [features], ...).
  deps_section=$(awk '
    /^\[.*dependencies\]/ { in_deps=1; next }
    /^\[/ { in_deps=0 }
    in_deps { print }
  ' "$manifest")

  for entry in "${ALLOW[@]}"; do
    dep="${entry%%:*}"
    allowed="${entry#*:}"
    # Matches `dep = "..."`, `dep = { ... }`, or `dep.workspace = true`.
    if echo "$deps_section" | grep -qE "^${dep}(\.[A-Za-z_]+)?[[:space:]]*="; then
      if [[ " $allowed " != *" $crate_name "* ]]; then
        echo "heavy-dep allow-list violation: $manifest depends on '$dep', which is only allowed in: $allowed"
        fail=1
      fi
    fi
  done
done

if [ "$fail" -ne 0 ]; then
  exit 1
fi

echo "heavy-dep allow-list OK"
