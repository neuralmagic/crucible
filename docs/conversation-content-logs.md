# Conversation content logs (OpenTelemetry GenAI)

An opt-in telemetry signal that exports the full agent conversation for a turn (prompts,
completions, reasoning, tool arguments, and tool output) as OpenTelemetry GenAI **log** records,
correlated to the turn's trace but stored separately. Off by default.

The per-turn tracing (the `openshell_turn` span with its transcript-grafted per-tool child spans)
carries **no conversation content** by design: each tool span keeps only a redacted one-line input
hint, never the full prompts, completions, tool arguments, or tool output. Span attributes are
size-capped and full agent I/O is sensitive. This signal adds that content back on an access
path you control, joined to the trace by id.

## What it emits

After a turn, the engine parses the agent's native session transcript (the same JSONL it already
fetches to graft tool spans) into GenAI log records, each stamped with the turn span's `trace_id` /
`span_id` so a logs backend (Loki, or an agent-observability tool) can join the conversation back to
the trace:

| Transcript entry | Log record |
| --- | --- |
| user prompt | `gen_ai.user.message` |
| system prompt | `gen_ai.system.message` |
| assistant turn (text + reasoning + tool calls with full args) | `gen_ai.assistant.message` |
| the final assistant turn (the completion) | `gen_ai.choice` |
| tool result (full output, `is_error`) | `gen_ai.tool.message` |

Every record carries `gen_ai.system` (`anthropic`) and, where the transcript has it,
`gen_ai.request.model`.

## Enabling it

Both must hold, or nothing is installed and the emit path is a no-op (zero cost, matching the trace
exporter):

| Variable | Purpose |
| --- | --- |
| `CRUCIBLE_TURN_TRACE_CONTENT` | The opt-in flag (`1` / `true` / `on`). Required because content export reverses the deliberate span content strip, so it is never a default. |
| `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | The OTLP/HTTP logs endpoint (Loki's OTLP ingest is HTTP-only, unlike the gRPC span exporter → Tempo). The logs-specific override; wins over the base and is used **verbatim** as the full URL (e.g. `http://collector.example:3100/otlp/v1/logs`, no path appended). |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Base OTLP endpoint, used for logs when no logs-specific one is set. Per the OTLP/HTTP convention `/v1/logs` is **appended** to it. |

## Redaction

Message bodies run through the same credential-scrub machinery as the span hints (env `NAME=value`
secrets, `Authorization:` headers, URL userinfo), applied per line so multi-line bodies keep their
structure. Controlled by the existing toggle:

| Variable | Purpose |
| --- | --- |
| `CRUCIBLE_TURN_TRACE_REDACT` | On by default; set `0` / `false` / `off` to disable (only when the store is already trusted). |

## Privacy

Point content logs at an **access-controlled logs store**, never the shared Tempo. The bodies are
confidential in the same way the raw session transcripts are.

The OTLP span and log emission above is public engine machinery; the MLflow turn-result exporter
is a component of the optional private control plane and is not part of this signal.
