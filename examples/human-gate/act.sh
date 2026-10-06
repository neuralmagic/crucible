#!/bin/sh
echo "$1 ${CRUCIBLE_TASK:-}" >> ACTED.log
printf '{"did": "%s", "task": "%s"}\n' "$1" "${CRUCIBLE_TASK:-}"
