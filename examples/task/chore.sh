#!/bin/sh
# Deterministic stand-in for an LLM turn: do one unit of "chore" work.
set -eu
lines=0
[ -f log.txt ] && lines=$(wc -l <log.txt)
echo "chore turn: $lines lines so far" >>log.txt
