#!/usr/bin/env bash
# Regenerate docs/loop-states.md and its diagram from the loop driver's own transition table.
#   scripts/loop-docs.sh          write the page, the dot source, and (with graphviz) the SVG
#   scripts/loop-docs.sh --check  fail if the page or the dot source is stale, write nothing
# The SVG is rendered locally with graphviz `dot` and committed, the same way docs/build-graph.png
# is; the check covers the text the SVG is rendered from, so a runner without graphviz still checks.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
page="$root/docs/loop-states.md"
graph="$root/docs/img/loop-states.dot"
svg="$root/docs/img/loop-states.svg"
bin=(cargo run --quiet --manifest-path "$root/Cargo.toml" -p crucible -- loop-states)

generated_page="$("${bin[@]}")"
generated_graph="$("${bin[@]}" --format dot)"

if [[ "${1:-}" == "--check" ]]; then
  stale=0
  diff -u "$page" <(printf '%s' "$generated_page") || stale=1
  diff -u "$graph" <(printf '%s' "$generated_graph") || stale=1
  if [[ $stale -ne 0 ]]; then
    echo "docs/loop-states.md or docs/img/loop-states.dot is stale; run scripts/loop-docs.sh" >&2
    exit 1
  fi
  exit 0
fi

printf '%s' "$generated_page" > "$page"
printf '%s' "$generated_graph" > "$graph"
if command -v dot >/dev/null; then
  dot -Tsvg -o "$svg" "$graph"
else
  echo "graphviz 'dot' not found; docs/img/loop-states.svg left as is" >&2
fi
