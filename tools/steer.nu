#!/usr/bin/env nu
# steer — push operator guidance into a running loop (consumed before the next iteration).
#
# Safe by construction: the gate is frozen, so a steer can only influence the agent's
# SOLUTION search (hypotheses to try, dead-ends to avoid), never the evaluation — the worst a
# bad steer does is waste an iteration. Appends to STEER.md (the live queue the
# loop reads + blanks each iteration) and to state/steer-log.jsonl (an audit trail, because
# a steered result is operator-influenced, not purely autonomous).

def main [
    text: string                        # the guidance text
    --source: string = "operator"       # who is steering (operator | claude)
    --steer-file: string = "STEER.md"   # live steer queue file
] {
    let ts = (date now | format date "%s" | into int)

    # Append to the live queue (loop injects the whole file next iteration, then blanks it).
    $"<!-- steer @($ts) by ($source) -->\n($text)\n\n" | save --append $steer_file

    # Append to the audit trail.
    mkdir state
    let entry = ({ts: $ts, source: $source, text: $text} | to json -r)
    $"($entry)\n" | save --append state/steer-log.jsonl

    print $"steer queued \(($source)\); the loop injects it before the next iteration"
}
