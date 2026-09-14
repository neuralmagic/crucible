# The opencode and pi harnesses

`crucible --harness opencode` or `--harness pi` (or `[agent].harness = "opencode" | "pi"`) runs
the turn with [OpenCode](https://opencode.ai) or [Pi](https://pi.dev) instead of Claude Code.
Both exist for one reason: an inference endpoint that speaks only OpenAI Chat Completions. Claude
Code speaks the Anthropic Messages API, and Codex dropped its `chat` wire API in early 2026 and
speaks only the Responses API, so neither can drive such an endpoint; these two can. Everything
downstream of the decoder is unchanged: the turn still emits `AgentEvent` NDJSON, and
keep/discard still reads the same `Result`.

```toml
[agent]
harness = "pi"
model = "qwen-3-8-27b"
```

## Auth and the endpoint

Both harnesses authenticate with a direct API key from the loop's environment, relayed into the
sandbox. Which API the turn speaks follows the environment:

| Environment | Endpoint |
| --- | --- |
| `OPENAI_BASE_URL` set (plus `OPENAI_API_KEY`) | that OpenAI-speaking endpoint; `CRUCIBLE_INFERENCE_WIRE_API=chat` (default) or `responses` picks the API |
| only `OPENAI_API_KEY` set | api.openai.com |
| `ANTHROPIC_API_KEY` set (plus optional `ANTHROPIC_BASE_URL`) | Anthropic Messages, at api.anthropic.com or the base URL |

The controller sets these from the provider a launch pinned: a custom provider with the
`chat_completions` protocol lands as `OPENAI_BASE_URL` + `OPENAI_API_KEY`. A turn with neither key
is refused before the sandbox starts.

The seeded config registers the endpoint as a provider named `crucible` and the model under it, so
the CLI never consults its own model catalog: opencode gets an `opencode.json` with the
`crucible` provider on `@ai-sdk/openai-compatible` (or `@ai-sdk/anthropic`) reading the key back
through `{env:OPENAI_API_KEY}`, pi gets a `models.json` with the `crucible` provider on
`openai-completions` (or `openai-responses`, `anthropic-messages`) reading `$OPENAI_API_KEY`.

## What differs from a claude turn

- **No session resume** on either harness; a logical session's second turn errors rather than
  silently starting fresh.
- **No OTEL.** Cost is the endpoint's own number when the CLI priced the model (it never does for
  the `crucible` provider) and otherwise the pricing-table estimate over the token usage the stream
  reports. OpenCode also spends one extra model request per turn generating the session's title,
  which its export counts in the turn's usage.
- **Egress.** Both add `api.openai.com` to the sandbox allowlist; a custom base URL's host is added
  per turn as it is for codex. A claude turn's allowlist is unchanged.
- **Pi has no MCP client.** The provisioning broker is unreachable from a pi turn, so a pack whose
  agent needs the broker's tools cannot run on pi. OpenCode takes the broker as a remote MCP server
  in its config, like codex.
- **Skills.** OpenCode discovers Claude Code's `.claude/skills`, so the toolbox lands there. Pi
  discovers `.agents/skills`, and the turn passes `--approve` so the workspace's project-local
  files are trusted without a prompt.
- **Reasoning effort.** Pi takes the shared `[agent].reasoning_effort` as `--thinking` (`max` maps
  to `xhigh`). OpenCode's equivalent (`--variant`) is provider-specific, so it is not passed.

## OpenCode's transcript is the session export

`opencode run --format json` mirrors its server's event stream to stdout and exits on the
session's idle signal. In a container that signal can overtake the last `text`/`step_finish`
events, so the stream is treated as display only. The sandbox invocation is a `bash` wrapper that
runs `opencode run`, then `opencode export <session>` into
`/sandbox/.local/share/opencode/crucible-export.json`; that export is the turn's transcript and
its only source of the result, token usage, and tool spans (`backfill_required`, the hermes
posture). A missing export is a loud transcript error, never a $0 success.

Pi's `--mode json` stream comes from its own process and is complete, so pi closes the turn from
the live stream and its session file under `$PI_CODING_AGENT_DIR/sessions` is trace garnish, like
claude's and codex's.

## Sandbox images

The sandbox needs the process that opens the socket on the egress binary allowlist, which
OpenShell matches by the kernel-resolved binary: `/usr/local/bin/opencode` must resolve to the
native opencode binary (npm's launcher never opens the socket itself), and pi is a node script,
so its allowlist carries `/usr/bin/node` and `/usr/local/bin/node` beside `/usr/local/bin/pi`.
The `opencode` and `pi` features of the controller's image feedstock install exactly that.
