# The opencode and pi harnesses

`crucible --harness opencode` or `--harness pi` (or `[agent].harness = "opencode" | "pi"`) runs
the turn with [OpenCode](https://opencode.ai) or [Pi](https://pi.dev) instead of Claude Code.
Use them for an inference endpoint that supports only OpenAI Chat Completions. Claude Code uses
the Anthropic Messages API. Codex removed its `chat` wire API in early 2026 and supports only
the Responses API. Neither can use such an endpoint, but OpenCode and Pi can. Everything after
the decoder is the same. The turn emits `AgentEvent` NDJSON, and keep/discard reads the same
`Result`.

```toml
[agent]
harness = "pi"
model = "qwen-3-8-27b"
```

## Auth and the endpoint

Both harnesses authenticate with an API key from the environment of the loop. The engine
relays the key into the sandbox. The environment selects the API of the turn:

| Environment | Endpoint |
| --- | --- |
| `OPENAI_BASE_URL` set (with `OPENAI_API_KEY`) | that OpenAI-compatible endpoint. `CRUCIBLE_INFERENCE_WIRE_API=chat` (default) or `responses` selects the API. |
| only `OPENAI_API_KEY` set | api.openai.com |
| `ANTHROPIC_API_KEY` set (with optional `ANTHROPIC_BASE_URL`) | Anthropic Messages, at api.anthropic.com or the base URL |

With a controller, these values come from the agent binding of the run in
`CRUCIBLE_INFERENCE`. The controller renders the binding from the provider of the launch
([ADR-0065](https://github.com/neuralmagic/crucible/blob/main/gov/adr/ADR-0065-the-controller-hands-the-engine-its-models-as-the-inference-document.toml)).
The engine sets the variables in the sandbox from the endpoint, protocol, and key of the
binding. A custom provider with the `chat_completions` protocol becomes `OPENAI_BASE_URL` and
`OPENAI_API_KEY`. If a turn has neither key, the engine rejects it before the sandbox starts.

The seeded configuration registers the endpoint as a provider named `crucible`, with the
model under it, so the CLI does not use its own model catalog. OpenCode gets an
`opencode.json` with the `crucible` provider on `@ai-sdk/openai-compatible` (or
`@ai-sdk/anthropic`), which reads the key through `{env:OPENAI_API_KEY}`. Pi gets a
`models.json` with the `crucible` provider on `openai-completions` (or `openai-responses`,
`anthropic-messages`), which reads `$OPENAI_API_KEY`.

## What differs from a claude turn

- **No session resume** on either harness. The second turn of a logical session fails. It
  does not start a new session.
- **No OTEL.** If the CLI priced the model, the cost is the number from the endpoint. The CLI
  never prices the `crucible` provider. In other cases, the cost is an estimate from the
  pricing table and the token usage in the stream. OpenCode also makes one more model request
  in each turn to generate the session title. Its export counts this request in the usage of
  the turn.
- **Egress.** Both add `api.openai.com` to the sandbox allowlist. For a custom base URL, the
  engine adds its host for each turn, as for codex. The allowlist of a claude turn does not
  change.
- **Pi has no MCP client.** A pi turn cannot reach the provisioning broker, so a pack whose
  agent needs the broker tools cannot run on pi. OpenCode gets the broker as a remote MCP
  server in its configuration, as codex does.
- **Skills.** OpenCode finds the `.claude/skills` directory of Claude Code, so the toolbox goes
  there. Pi finds `.agents/skills`. The turn passes `--approve`, so Pi trusts the project-local
  files of the workspace without a prompt.
- **Reasoning effort.** Pi gets the shared `[agent].reasoning_effort` as `--thinking` (`max`
  becomes `xhigh`). The OpenCode equivalent (`--variant`) is specific to each provider, so the
  engine does not pass it.

## OpenCode's transcript is the session export

`opencode run --format json` copies the event stream of its server to stdout and exits on the
idle signal of the session. In a container, that signal can arrive before the last `text` or
`step_finish` events, so the engine uses the stream only for display. The sandbox command is a
`bash` wrapper. It runs `opencode run`, then `opencode export <session>` into
`/sandbox/.local/share/opencode/crucible-export.json`. That export is the transcript of the
turn. It is the only source of the result, the token usage, and the tool spans
(`backfill_required`, as for hermes). A missing export is a transcript error, never a $0
success.

The `--mode json` stream of Pi comes from its own process and is complete. Pi ends the turn
from the live stream. Its session file under `$PI_CODING_AGENT_DIR/sessions` only adds detail
to the trace, as for claude and codex.

## Sandbox images

The process that opens the socket must be on the egress binary allowlist of the sandbox.
OpenShell matches the binary that the kernel resolves. `/usr/local/bin/opencode` must resolve
to the native opencode binary, because the npm launcher does not open the socket. Pi is a node
script, so its allowlist has `/usr/bin/node` and `/usr/local/bin/node` in addition to
`/usr/local/bin/pi`. The `opencode` and `pi` features of the image feedstock of the controller
install these binaries.
