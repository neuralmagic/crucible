#!/usr/bin/env bash
# Reads changed paths on stdin and prints, as GitHub outputs, which CI job groups they need.
# `--all` selects every group without reading anything.
set -euo pipefail

rust=false
ui=false
lint=false

if [ "${1:-}" = "--all" ]; then
  rust=true
else
  while IFS= read -r path; do
    case "$path" in
      "") ;;
      crucible-controller/ui/*) ui=true ;;
      gov/* | docs/rfc/* | docs/adr/*) lint=true ;;
      .github/workflows/ci.yml) rust=true ;;
      .github/workflows/* | crucible-desktop/*) ;;
      */*) rust=true ;;
      *.md | LICENSE-*) ;;
      *) rust=true ;;
    esac
  done
fi

if [ "$rust" = true ]; then
  ui=true
  lint=true
fi

printf 'rust=%s\nui=%s\nlint=%s\n' "$rust" "$ui" "$lint"
