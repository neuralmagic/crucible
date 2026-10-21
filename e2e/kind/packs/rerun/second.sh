#!/bin/sh
set -eu
test "$(cat first.out)" = made-by-first
test "$(cat ../state/files/first/FIRST.txt)" = captured
grep -q '"mode": *"fixed"' "$CRUCIBLE_INPUTS_FILE"
echo '{"ok": true}'
