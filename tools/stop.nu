#!/usr/bin/env nu
# stop — park a running loop. Writes a cross-process stop signal the loop polls at each
# iteration checkpoint; it then keeps its best, writes `finished`, and exits.
#
# Counterpart to `steer`: steer nudges the search, stop ends the run cleanly so it can be
# resumed later with `crucible --resume`. Run from the repo root (paths are CWD-relative,
# matching the loop's state/ dir).

def main [
    --source: string = "operator"                  # who is stopping the run (audit trail)
    --control-file: string = "state/control.json"  # control file the loop polls
] {
    let ts = (date now | format date "%s" | into int)

    # Same shape crucible's viewer writes (the two writers can't share a crate).
    mkdir state
    {stop: true, kill_agent: true, seq: $ts} | to json -r | save -f $control_file

    # Audit trail, mirroring steer-log.jsonl.
    let entry = ({ts: $ts, source: $source, action: "stop"} | to json -r)
    $"($entry)\n" | save --append state/control-log.jsonl

    print $"stop signalled \(($source)\); the loop will wrap up the current step, keep its best, and exit"
}
