#!/usr/bin/env nu
# session — read-only, domain-neutral snapshot of a running (or finished) crucible loop, as
# one JSON blob.
#
# Collates what you'd otherwise dig for by hand (process state, the folded session log, results
# log, current candidate, any escalation, recent kept commits) so an operator — or an MCP server
# wrapping this — can introspect a live session without archaeology. Purely read-only; pair with
# `steer` to act on what you see. Domain-specific live state (e.g. a live rig's image/configmap)
# is the domain's own concern, owned by the domain's own tools.

# Fold the NDJSON session log into a cheap, authoritative status: where the run is, its
# budget, last decision, whether it finished. Parsed independently of crucible — the wire
# shape IS the contract. Returns null when the log doesn't exist yet. (nushell is built for
# exactly this: structured records in, a record out.)
def fold-session [path: string] {
    if not ($path | path exists) { return null }
    mut acc = {
        events: 0, goal: null, gate: null, phase: null, iter: null,
        last_row: null, budget: null, best_score: null, finished: false
    }
    for line in (open --raw $path | lines) {
        let line = ($line | str trim)
        if ($line | is-empty) { continue }
        let v = (try { $line | from json } catch { null })
        # Skip anything that didn't parse into a record (garbage lines, bare scalars): an
        # optional cell path still hard-errors on a non-record, so guard the type here.
        if not (($v | describe) | str starts-with "record") { continue }
        $acc.events = $acc.events + 1
        match ($v.kind? | default "") {
            "start" => {
                $acc.goal = ($v.goal? | default null)
                $acc.gate = ($v.gate? | default null)
            }
            "phase" => {
                $acc.phase = ($v.phase? | default null)
                $acc.iter = ($v.iter? | default null)
            }
            "row" => { $acc.last_row = ($v.row? | default null) }
            "budget" => {
                $acc.budget = {spent: ($v.spent? | default null), elapsed_secs: ($v.elapsed_secs? | default null)}
            }
            "summary" => { $acc.best_score = ($v.best_score? | default null) }
            "finished" => { $acc.finished = true }
            _ => {}
        }
    }
    $acc
}

# Best-effort external: returns trimmed stdout, swallows failure (empty on any error).
def cmd-out [prog: string, ...args: string] {
    ^$prog ...$args | complete | get stdout | str trim
}

def read-file [path: string] {
    if ($path | path exists) { open --raw $path } else { null }
}

def main [
    --workspace: string = "workspace"            # agent workspace to inspect (e.g. a pack's workspace dir)
    --session: string = "state/session.jsonl"    # session event log from crucible --ui stream
] {
    let ws = $workspace

    let loop_running = ((cmd-out "pgrep" "-f" "crucible --manifest") | is-not-empty)
    let agent_running = ((cmd-out "pgrep" "-f" "claude --permission-mode bypassPermissions") | is-not-empty)

    {
        loop_running: $loop_running,
        agent_running: $agent_running,
        session: (fold-session $session),
        results_md: (read-file ([$ws "RESULTS.md"] | path join)),
        candidate_md: (read-file ([$ws "CANDIDATE.md"] | path join)),
        escalation: (read-file ([$ws "ESCALATION.json"] | path join)),
        workspace_head: (cmd-out "git" "-C" $ws "log" "--oneline" "-n" "8"),
    } | to json
}
