#!/usr/bin/env bash
# Duplicate-generation guard for the reqwest/rustls/hyper/ring/h2 dependency cluster.
#
# `cargo tree -d` lists every crate with more than one resolved version in the lock file. We
# grep that down to the http/tls cluster this repo has historically accumulated dup generations
# in (aws-sdk-s3's hyper/rustls stack vs. everything else's newer reqwest/rustls stack) and
# assert the set doesn't grow. Shrinking (de-duping) is always fine and does not need this file
# updated, but it's polite to trim the snapshot below when you land a de-dup so the guard stays
# tight.
#
# `--check` (the default) compares the current tree against tools/dep-dups.snapshot.
# `--snapshot` regenerates that file from the current tree — run it after a deliberate dep
# change. Both modes call the exact same `generate_actual` so the snapshot can never drift from
# what the check reads: no separately-typed heredoc to fall out of sync.
#
# If you deliberately introduce a new duplicate generation, run `--snapshot` in the same PR and
# explain why in the PR description.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SNAPSHOT_FILE="$SCRIPT_DIR/dep-dups.snapshot"

# Deterministic invocation:
#   --locked        never silently re-resolves the lockfile (a bare `cargo tree` would).
#   --workspace     every member's dependency graph, not just the one `cargo` would default to
#                   from cwd.
#   -e normal,build,dev
#                   explicit edge kinds so the set can't shift under us if cargo's own default
#                   ever changes.
#   --all-features  matches what CI's `cargo clippy --all-features` actually compiles.
#   --color=never   CI sets CARGO_TERM_COLOR=always, which wraps the trailing "(*)" marker in
#                   ANSI escapes. That broke the `sed` strip below so every line came out
#                   "not in the snapshot" even when it was — the actual bug this file's rewrite
#                   fixes. --color=never pins the output regardless of the caller's terminal
#                   env.
generate_actual() {
  cargo tree -d --locked --workspace -e normal,build,dev --all-features --color=never 2>/dev/null \
    | grep -E '^[a-zA-Z]' \
    | grep -E 'reqwest|rustls|hyper|ring|h2' \
    | sed -E 's/ \(\*\)$//' \
    | sort -u
}

if [ "${1:-}" = "--snapshot" ]; then
  generate_actual > "$SNAPSHOT_FILE"
  echo "Wrote $(wc -l < "$SNAPSHOT_FILE" | tr -d ' ') duplicated generation(s) to $SNAPSHOT_FILE"
  exit 0
fi

ACTUAL=$(generate_actual)
EXPECTED=$(sort -u "$SNAPSHOT_FILE")

NEW=$(comm -13 <(echo "$EXPECTED") <(echo "$ACTUAL") || true)

if [ -n "$NEW" ]; then
  echo "New duplicate dependency generation(s) not in the snapshot:"
  echo "$NEW"
  echo
  echo "Either de-dupe it, or if it's an intentional tradeoff, regenerate the snapshot with"
  echo "'./tools/check-dep-dups.sh --snapshot' and say why in the PR description."
  exit 1
fi

echo "dep-dup guard OK ($(echo "$ACTUAL" | wc -l | tr -d ' ') duplicated generations, none new)"
