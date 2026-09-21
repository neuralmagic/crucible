#!/bin/sh
echo "$1" >> ROUTED.log
printf '{"did": "%s"}\n' "$1"
