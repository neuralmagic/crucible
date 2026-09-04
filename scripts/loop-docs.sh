#!/usr/bin/env bash
# Regenerate docs/loop-machine.md from the loop machine's own vocabulary.
#   scripts/loop-docs.sh          write the page
#   scripts/loop-docs.sh --check  fail if the page is stale, write nothing
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
page="$root/docs/loop-machine.md"

generated="$(cargo run --quiet --manifest-path "$root/Cargo.toml" -p crucible -- loop-reference)"

if [[ "${1:-}" == "--check" ]]; then
  if ! diff -u "$page" <(printf '%s' "$generated"); then
    echo "docs/loop-machine.md is stale; run scripts/loop-docs.sh" >&2
    exit 1
  fi
  exit 0
fi

printf '%s' "$generated" > "$page"
