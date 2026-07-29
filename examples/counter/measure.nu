#!/usr/bin/env nu
# The Judge: score = the integer in value.txt; higher wins; solved at >= 5. Emits the
# crucible measure contract (last stdout line = {valid, score, solved?, note?}). The engine
# injects CRUCIBLE_BASELINE_SCORE / CRUCIBLE_BEST_SCORE in env (unused here, but available).

let v = (open value.txt | str trim | into int)
{valid: true, score: $v, solved: ($v >= 5), note: $"value=($v)"} | to json -r
