#!/usr/bin/env nu
# escalate — the agent's escape hatch: declare that the HARNESS (not the task) is the
# blocker and end the run for human review.
#
# The agent cannot change its own evaluation (prevents reward-hacking). The honest valve: if,
# after trying, the agent concludes the gate cannot measure what it needs (a required signal
# is dead, the workload doesn't exercise the issue, the rig lacks a capability), it calls
# `escalate` with evidence instead of submitting a change or gaming the metric. The loop
# detects the marker, stops, restores the rig, and surfaces the report for a human to fix the
# harness.
#
# This is NOT "I couldn't improve it" (that's a normal discard) — it's "no change here can be
# evaluated". Use it sparingly and back it with evidence (e.g. inspect-rig output).
#
# Writes ./ESCALATION.json in the workspace; the loop halts on it.

def main [
    --category: string         # harness-limitation | infeasible | needs-info
    --reason: string           # what the gate cannot measure and what you tried (>= 20 chars)
    --evidence: string = ""    # proof backing the escalation (e.g. inspect-rig output across configs)
] {
    let categories = ["harness-limitation" "infeasible" "needs-info"]
    if $category not-in $categories {
        print -e $"--category must be one of: ($categories | str join ', ')"
        exit 2
    }
    if (($reason | default "" | str trim | str length) < 20) {
        print -e "--reason too thin; explain what the gate cannot measure and what you tried"
        exit 1
    }
    if ($evidence | str trim | is-empty) {
        print -e "WARN: no --evidence given; escalations should cite proof (e.g. inspect-rig output across configs)"
    }

    let ts = (date now | format date "%s" | into int)
    {category: $category, reason: $reason, evidence: $evidence, ts: $ts} | to json | save -f ESCALATION.json
    print $"escalated \(($category)\); the loop will stop and surface this for human review"
}
