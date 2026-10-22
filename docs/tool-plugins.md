# Agent tool plugins

Every agent turn's tool calls and provider retries pass through a chain of plugins. A plugin can
tag a call for the plugins after it, stop the turn, or report when the turn ends. The chain only
reads the agent's event stream; it never changes what the agent does or what the run log records.

The chain is declared per pack, in order:

```toml
[[agent.tool_plugins]]
plugin = "repeat_guard"
limit  = 25

[[agent.tool_plugins]]
plugin = "classify"

[[agent.tool_plugins]]
plugin = "tool_stats"
```

Leaving `tool_plugins` out runs exactly that chain. `tool_plugins = []` runs no plugins at all.
An unknown plugin name or parameter fails the manifest check and names the plugins that exist.

## Plugins

### `repeat_guard`

Stops the turn when the agent goes round the same cycle of tool calls `limit` times in a row
(`limit` is required, at least 1). A cycle is one call, or two or three calls taking turns. Two
calls are the same when their tool, arguments, result and failure all match, so a poll whose
output changes counts as progress, and the same command in a different directory is a different
call. A call that fails the same way every time is caught too, and so is an agent that runs a
build, reads the same error, and runs the build again.

A stopped turn fails as an agent transport failure, which the executor retries in a fresh turn up
to its transport retry limit. A wedge is usually a property of one session rather than of the
task, so the fresh turn tends to succeed; when retries run out the task fails and a settled join
goes on without it. The stop shows in the run log as a `tool_plugin` error naming each call in
the cycle.

The default of 25 comes from 1,198 recorded turns: healthy turns peaked at 14 identical calls in a
row (p99 5), and the two wedged turns in the sample reached 549 and 746. Any limit from 15 to 40
separated them. Raise it for a pack whose agents legitimately repeat one call with the same
output, such as polling a long build with a status command that prints nothing new.

### `classify`

Tags each call as `read`, `write`, `exec`, `build`, `network`, `mcp`, `agent` or `other`, from the
tool name and, for shell calls, the programs the command runs. It stops nothing; `tool_stats`
counts the tags. Place it before `tool_stats`.

### `tool_stats`

At the end of the turn, logs a `tool_stats` event (and a tracing event) with the turn's tool use:

```json
{
  "calls": 37,
  "retries": 4,
  "longest_fail_streak": 2,
  "tools": {
    "Bash": { "calls": 24, "failed": 3, "ms": 81234 },
    "Read": { "calls": 11, "failed": 0, "ms": 412 },
    "Write": { "calls": 2, "failed": 0, "ms": 35 }
  },
  "classes": { "build": 9, "exec": 15, "read": 11, "write": 2 }
}
```

`ms` is the time from a call to its result, so it is measured only for harnesses that report the
two separately (Claude Code); the others report 0. `retries` counts provider retries and
`longest_fail_streak` the longest run of failed calls in a row.

## Compared with OpenHands

OpenHands ships a stuck detector in its agent SDK
([`stuck_detector.py`](https://github.com/OpenHands/software-agent-sdk/blob/c8a65f0db7c4e5b455a124989c3a6a0a79fcd9e9/openhands-sdk/openhands/sdk/conversation/stuck_detector.py)),
on by default, that watches the last 20 events since the user last spoke:

| Pattern | OpenHands | `repeat_guard` |
|---|---|---|
| Same action, same observation | stuck at 4 | stuck at `limit` (25) |
| Same action, same error | nudge at 3, stuck at 4 | the call-and-result pair rule, at `limit` |
| Two actions alternating with the same observations | stuck at 6 | a two-call cycle, at `limit` |
| Agent messages with no user input | stuck at 3 | not applicable: a turn has no user |
| Context window errors in a loop | stubbed out (returns false) | not applicable |

Both compare calls on their content and outcome, not on ids or timing. The differences come from
how the agents run:

- **Thresholds.** OpenHands' defaults suit an interactive agent a user is watching. A crucible
  turn runs unattended to completion, and agents repeat themselves more than that legitimately:
  in the 1,198 recorded turns above, up to 36 of 999 healthy turns had 4 identical calls in a row
  and up to 7 had 6. (These are upper bounds: the recorded logs carry calls but not their
  output, and OpenHands also requires the observations to match.) At 25, `repeat_guard` stops
  none of those turns and both real wedges.
- **Recovery.** OpenHands can add a message to the conversation, so on a repeating error it first
  nudges the agent to change course, and on a stuck verdict stops the run in a `STUCK` state.
  Crucible drives the harness's CLI and cannot speak to a running turn, so it ends the turn and
  the executor retries the task in a fresh one.
- **Cycles.** `repeat_guard` also catches three calls taking turns, which OpenHands does not. In
  the recorded turns, two calls alternating peaked at 3 cycles and three-call cycles never
  repeated, so the same `limit` of 25 cycles applies to all of them.

## What the chain sees

The chain always reads full tool inputs and results, whatever `CRUCIBLE_SESSION_TOOL_IO` says: when
the run log is compact, the engine decodes the agent's stream a second time for the chain only.
Every harness (Claude Code, Codex, OpenCode, Pi) marks a failed call with the `failed` flag on its
`tool` events, from its own error signal.

## Stopping a turn

On the local and command backends the engine kills the agent, with its whole process group when
the turn runs under a deadline. On the openshell backend it cancels only the agent's exec, so the
sandbox is still torn down as usual.

## Adding a plugin

A plugin implements `ToolPlugin` in `crucible/src/agent/tool_chain.rs`: `on_tool` and `on_retry`
return `Flow::Continue` or `Flow::Stop` with a typed `StopReason`, and `report` returns an
end-of-turn label and JSON value. Add its manifest shape to `ToolPluginSpec` in
`crucible/src/manifest/tool_plugins.rs`, and add it to the default chain only with run data that
shows it catches something without stopping healthy turns.
