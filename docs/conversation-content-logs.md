# Conversation content logs (OpenTelemetry GenAI)

This optional telemetry signal exports the full agent conversation of a turn (prompts,
completions, reasoning, tool arguments, and tool output) as OpenTelemetry GenAI **log**
records. The records link to the trace of the turn, but they are stored separately. The
signal is disabled by default.

The tracing of each turn (the `openshell_turn` span, with a child span for each tool from the
transcript) contains **no conversation content**. Each tool span keeps only a redacted
one-line input hint. It does not keep the full prompts, completions, tool arguments, or tool
output. Span attributes have a size limit, and full agent input and output is sensitive. This
signal exports that content to a store that you control, linked to the trace by id.

## What it emits

After a turn, the engine parses the native session transcript of the agent (the same JSONL
that it reads to make the tool spans) into GenAI log records. Each record has the `trace_id`
and `span_id` of the turn span, so a logs backend (Loki, or an agent-observability tool) can
join the conversation to the trace:

| Transcript entry | Log record |
| --- | --- |
| user prompt | `gen_ai.user.message` |
| system prompt | `gen_ai.system.message` |
| assistant turn (text + reasoning + tool calls with full args) | `gen_ai.assistant.message` |
| the final assistant turn (the completion) | `gen_ai.choice` |
| tool result (full output, `is_error`) | `gen_ai.tool.message` |

Every record has `gen_ai.system` (`anthropic`). If the transcript has the model, the record
also has `gen_ai.request.model`.

## Enabling it

Both conditions below are necessary. If one is missing, the engine installs no exporter, and
the emit path does nothing and costs nothing, as for the trace exporter:

| Variable | Purpose |
| --- | --- |
| `CRUCIBLE_TURN_TRACE_CONTENT` | The enable flag (`1` / `true` / `on`). It is required, because content export reverses the removal of content from spans, so it is never the default. |
| `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | The OTLP/HTTP logs endpoint. Loki accepts OTLP only over HTTP, but the span exporter to Tempo uses gRPC. This variable is specific to logs, and it overrides the base endpoint. The engine uses it **without change** as the full URL (for example, `http://collector.example:3100/otlp/v1/logs`, with no path added). |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | The base OTLP endpoint. The engine uses it for logs if no logs endpoint is set. As the OTLP/HTTP convention specifies, the engine **adds** `/v1/logs` to it. |

## Redaction

The engine removes credentials from message bodies with the same code as for the span hints
(environment `NAME=value` secrets, `Authorization:` headers, URL userinfo). It applies the
redaction to each line, so multi-line bodies keep their structure. This variable controls it:

| Variable | Purpose |
| --- | --- |
| `CRUCIBLE_TURN_TRACE_REDACT` | Enabled by default. Set `0` / `false` / `off` to disable it. Disable it only if the store is trusted. |

## Privacy

Send content logs to an **access-controlled logs store**, not to the shared Tempo. The bodies
are as confidential as the raw session transcripts.

The engine emits the OTLP spans and logs above. The MLflow turn-result exporter is part of the
controller (`crucible-controller/src/runs/mlflow.rs`). It is separate from this signal.
