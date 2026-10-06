#!/bin/sh
echo "$1" >> ACTED.log
printf '{"did": "%s"}\n' "$1"
