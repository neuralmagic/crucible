#!/usr/bin/env nu
# The deterministic proposer (the `command` agent backend). Stands in for an LLM turn: edit
# the workspace toward the goal. Here: value.txt += 1. Runs with cwd = workspace.

let v = (open value.txt | str trim | into int)
($v + 1 | into string) | save -f value.txt
