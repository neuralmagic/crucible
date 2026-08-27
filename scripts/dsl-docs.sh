#!/usr/bin/env bash
# Regenerate docs/dsl-reference.md from the compiler's own DSL tables.
#   scripts/dsl-docs.sh          write the page
#   scripts/dsl-docs.sh --check  fail if the page is stale, write nothing
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
page="$root/docs/dsl-reference.md"

generated="$(cargo run --quiet --manifest-path "$root/Cargo.toml" -p crucible -- plan dsl-reference)"

if [[ "${1:-}" == "--check" ]]; then
  if ! diff -u "$page" <(printf '%s' "$generated"); then
    echo "docs/dsl-reference.md is stale; run scripts/dsl-docs.sh" >&2
    exit 1
  fi
  exit 0
fi

printf '%s' "$generated" > "$page"
