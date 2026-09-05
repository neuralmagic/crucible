#!/usr/bin/env bash
# Regenerate the control-state pages and their diagrams from the engine's own transition
# tables: docs/loop-states.md (the run loop) and docs/plan-states.md (the plan executor).
#   scripts/state-docs.sh          write the pages, the dot sources, and (with graphviz) the SVGs
#   scripts/state-docs.sh --check  fail if a page or a dot source is stale, write nothing
# The SVGs are rendered locally with graphviz `dot` and committed, the same way
# docs/build-graph.png is; the check covers the text they are rendered from, so a runner
# without graphviz still checks.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
crucible=(cargo run --quiet --manifest-path "$root/Cargo.toml" -p crucible --)

# page-or-dot path, then the command that produces it.
outputs=(
  "docs/loop-states.md|loop-states"
  "docs/img/loop-states.dot|loop-states --format dot"
  "docs/plan-states.md|plan states"
  "docs/img/plan-task-states.dot|plan states --format dot --graph task"
  "docs/img/plan-states.dot|plan states --format dot --graph plan"
)

stale=0
for entry in "${outputs[@]}"; do
  path="$root/${entry%%|*}"
  read -r -a args <<< "${entry#*|}"
  generated="$("${crucible[@]}" "${args[@]}")"
  if [[ "${1:-}" == "--check" ]]; then
    diff -u "$path" <(printf '%s' "$generated") || stale=1
  else
    printf '%s' "$generated" > "$path"
  fi
done

if [[ "${1:-}" == "--check" ]]; then
  if [[ $stale -ne 0 ]]; then
    echo "a control-state page or dot source is stale; run scripts/state-docs.sh" >&2
    exit 1
  fi
  exit 0
fi

if command -v dot >/dev/null; then
  for graph in loop-states plan-task-states plan-states; do
    dot -Tsvg -o "$root/docs/img/$graph.svg" "$root/docs/img/$graph.dot"
  done
else
  echo "graphviz 'dot' not found; the SVGs under docs/img were left as they are" >&2
fi
