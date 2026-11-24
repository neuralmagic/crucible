#!/bin/sh
# Counts rounds in the workspace and asks for another until the third.
n=$(($(cat ROUNDS 2>/dev/null || echo 0) + 1))
echo "$n" > ROUNDS
if [ "$n" -lt 3 ]; then go=true; else go=false; fi
printf '{"go": %s, "points": %s}\n' "$go" "$n"
