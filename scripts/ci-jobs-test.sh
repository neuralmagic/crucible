#!/usr/bin/env bash
# Checks scripts/ci-jobs.sh against paths whose job groups are known.
set -euo pipefail

pick="$(dirname "$0")/ci-jobs.sh"
failed=0

expect() {
  local want="$1" got
  shift
  got=$(printf '%s\n' "$@" | "$pick" | tr '\n' ' ')
  if [ "$got" != "$want " ]; then
    echo "paths: $*" >&2
    echo "  want: $want" >&2
    echo "  got:  $got" >&2
    failed=1
  fi
}

all="rust=true ui=true lint=true"
none="rust=false ui=false lint=false"

expect "$all" crucible/src/plan/exec.rs
expect "$all" Cargo.lock
expect "$all" Cargo.toml
expect "$all" .github/workflows/ci.yml
expect "$all" .github/actions/postgres/action.yml
expect "$all" scripts/ci-jobs.sh
expect "$all" crucible-controller/src/spa.rs
expect "$all" crucible-controller/migrations/0049_x.sql
expect "$all" docs/crucible-contract.md
expect "$all" docs/hand-rolled-pipelines.md
expect "$all" examples/counter/crucible.toml
expect "$all" editors/vscode/syntaxes/crucible-starlark.tmLanguage.json
expect "$all" vendor/rune/src/lib.rs
expect "$all" crucible/README.md
expect "$all" crucible-controller/ui/src/App.tsx crucible/src/lib.rs
expect "$all" README.md justfile

expect "rust=false ui=true lint=false" crucible-controller/ui/src/App.tsx
expect "rust=false ui=true lint=false" crucible-controller/ui/package.json README.md
expect "rust=false ui=false lint=true" gov/rfc/RFC-0006.toml docs/rfc/RFC-0006.md
expect "rust=false ui=false lint=true" docs/adr/ADR-0075.md
expect "rust=false ui=true lint=true" docs/adr/ADR-0075.md crucible-controller/ui/src/App.tsx

expect "$none" README.md
expect "$none" CONTRIBUTING.md LICENSE-MIT
expect "$none" crucible-desktop/src/lib.rs crucible-desktop/Cargo.lock
expect "$none" .github/workflows/desktop.yml .github/workflows/release.yml
expect "$none" ""

got=$("$pick" --all </dev/null | tr '\n' ' ')
[ "$got" = "$all " ] || { echo "--all gave: $got" >&2; failed=1; }

exit "$failed"
