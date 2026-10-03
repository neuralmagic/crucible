#!/bin/sh
set -eu
test "$(cat nested/deep/marker.txt)" = "pack-delivered"
for f in bulk/a.txt bulk/b.txt bulk/c.txt; do
    test "$(wc -c <"$f")" -eq 512000
done
test -x check.sh
echo '{"ok":true}'
