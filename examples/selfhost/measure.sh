#!/bin/sh
# The Judge: the harness crate's tests and clippy (-D warnings) must pass, then the bench's last stdout line IS the
# measure contract ({valid, score, note}; score = ns/line, lower wins). Test/compile failures
# report invalid instead of a score so a broken candidate is discarded, not kept or crashed on.
set -u
export CARGO_TERM_COLOR=never
if ! out=$(cargo test -p crucible-harness -q 2>&1); then
    note=$(printf '%s' "$out" | grep -E '^(error|test .* FAILED|failures:)' | head -5 | tr '\n' ' ' | cut -c1-400)
    printf '{"valid":false,"score":0,"note":%s}\n' "$(printf '%s' "tests failed: $note" | jq -Rs .)"
    exit 0
fi
if ! out=$(cargo clippy -p crucible-harness --all-targets -q -- -D warnings 2>&1); then
    note=$(printf '%s' "$out" | grep -E '^(error|warning)' | head -5 | tr '\n' ' ' | cut -c1-400)
    printf '{"valid":false,"score":0,"note":%s}\n' "$(printf '%s' "clippy failed: $note" | jq -Rs .)"
    exit 0
fi
if ! out=$(cargo bench -p crucible-harness --bench stream_json -q 2>&1); then
    note=$(printf '%s' "$out" | grep -E '^error' | head -5 | tr '\n' ' ' | cut -c1-400)
    printf '{"valid":false,"score":0,"note":%s}\n' "$(printf '%s' "bench failed to build: $note" | jq -Rs .)"
    exit 0
fi
printf '%s\n' "$out" | tail -1
