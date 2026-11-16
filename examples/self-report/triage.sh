#!/bin/sh
set -eu
cp "inbox/$1.json" verdicts.json
printf '{"lane": "%s"}\n' "$1"
