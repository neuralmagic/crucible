// The session-log wire format as the SPA sees it over SSE. Mirrors `crucible::session::SessionEvent`
// (the `kind`-tagged NDJSON the loop appends to session.jsonl) and its nested
// `crucible::event::AgentEvent`. We model only the fields the live feed renders; every arm keeps an
// `unknown` fallback so a forward-compat kind (or a torn line) is surfaced, never dropped. There is
// no `as` anywhere — the lint bans type assertions — so every field is pulled through a real guard.

import { isRecord } from '../json';

/// The status snapshot the relay forwards on connect + every ~10s
/// (`crucible::control::StatusSnapshot`). `best_score`/`max_cost` are skipped when absent.
export interface LiveStatus {
  phase: string;
  iter: number;
  best_score?: number;
  spend: number;
  paused: boolean;
  max_cost?: number;
}

/// A plain-serde mirror of `RowWire`, narrowed to what the feed shows.
export interface RowWire {
  iter: number;
  decision: string;
  note: string;
  detail: string;
  score?: number;
  total?: number;
  phase?: string;
}

/// One nested agent event (`crucible::event::AgentEvent`). Text/thinking deltas stream token-by-token
/// and get coalesced upstream (see feed.ts); the rest render as single rows.
export type AgentEvent =
  | { kind: 'meta'; model: string }
  | { kind: 'log'; level: string; label: string; value?: string }
  | { kind: 'gate'; phase: string; name: string; status: string; error?: string }
  | { kind: 'init'; model: string; tools: number; agents: number }
  | { kind: 'thinking'; delta: string }
  | { kind: 'text'; delta: string }
  | { kind: 'tool'; name: string; summary: string; subagent: boolean }
  | { kind: 'tokens' }
  | { kind: 'retry'; attempt: number; max: number; error: string }
  | { kind: 'error'; error_type: string; message: string }
  | { kind: 'result'; subtype: string; turns: number; cost_usd: number }
  | { kind: 'otel_summary'; cost_usd: number; total: number; api_requests: number }
  | { kind: 'exit'; code: number }
  | { kind: 'raw'; text: string; stream: string }
  | { kind: 'unknown'; raw: string };

/// One session-log line, discriminated on `kind`, with an `unknown` fallback for anything we don't
/// model (a newer engine kind, a non-JSON frame). `unknown.raw` carries the verbatim line.
export type SessionLine =
  | { kind: 'start'; goal: string; gate: string; model: string; namespace: string; iters_total: number; max_cost: number; max_secs: number }
  | { kind: 'phase'; phase: string; iter: number }
  | { kind: 'note'; msg: string }
  | { kind: 'row'; row: RowWire; solved: boolean }
  | { kind: 'agent_start'; iter: number }
  | { kind: 'agent'; event: AgentEvent }
  | { kind: 'agent_done' }
  | { kind: 'budget'; spent: number; elapsed_secs: number }
  | { kind: 'summary'; gate: string; best_score: number }
  | { kind: 'escalation'; category: string; reason: string; evidence: string }
  | { kind: 'segment'; fingerprint: string; baseline_score: number; regime: string }
  | { kind: 'identity'; digest: string }
  | { kind: 'finished' }
  | { kind: 'shutdown'; outcome: string; reason: string }
  | { kind: 'unknown'; raw: string };

function str(o: Record<string, unknown>, k: string, d = ''): string {
  const v = o[k];
  return typeof v === 'string' ? v : d;
}

function optNum(o: Record<string, unknown>, k: string): number | undefined {
  const v = o[k];
  return typeof v === 'number' ? v : undefined;
}

function numOr(o: Record<string, unknown>, k: string, d: number): number {
  return optNum(o, k) ?? d;
}

function boolOr(o: Record<string, unknown>, k: string, d: boolean): boolean {
  const v = o[k];
  return typeof v === 'boolean' ? v : d;
}

function parseRow(v: unknown): RowWire {
  if (!isRecord(v)) return { iter: 0, decision: '', note: '', detail: '' };
  return {
    iter: numOr(v, 'iter', 0),
    decision: str(v, 'decision'),
    note: str(v, 'note'),
    detail: str(v, 'detail'),
    score: optNum(v, 'score'),
    total: optNum(v, 'total'),
    phase: typeof v.phase === 'string' ? v.phase : undefined,
  };
}

function parseAgent(v: unknown): AgentEvent {
  if (!isRecord(v) || typeof v.kind !== 'string') {
    return { kind: 'unknown', raw: safeStringify(v) };
  }
  switch (v.kind) {
    case 'meta':
      return { kind: 'meta', model: str(v, 'model') };
    case 'log':
      return { kind: 'log', level: str(v, 'level'), label: str(v, 'label'), value: typeof v.value === 'string' ? v.value : undefined };
    case 'gate':
      return { kind: 'gate', phase: str(v, 'phase'), name: str(v, 'name'), status: str(v, 'status'), error: typeof v.error === 'string' ? v.error : undefined };
    case 'init':
      return { kind: 'init', model: str(v, 'model'), tools: numOr(v, 'tools', 0), agents: numOr(v, 'agents', 0) };
    case 'thinking':
      return { kind: 'thinking', delta: str(v, 'delta') };
    case 'text':
      return { kind: 'text', delta: str(v, 'delta') };
    case 'tool':
      return { kind: 'tool', name: str(v, 'name'), summary: str(v, 'summary'), subagent: boolOr(v, 'subagent', false) };
    case 'tokens':
      return { kind: 'tokens' };
    case 'retry':
      return { kind: 'retry', attempt: numOr(v, 'attempt', 0), max: numOr(v, 'max', 0), error: str(v, 'error') };
    case 'error':
      return { kind: 'error', error_type: str(v, 'error_type'), message: str(v, 'message') };
    case 'result':
      return { kind: 'result', subtype: str(v, 'subtype'), turns: numOr(v, 'turns', 0), cost_usd: numOr(v, 'cost_usd', 0) };
    case 'otel_summary':
      return { kind: 'otel_summary', cost_usd: numOr(v, 'cost_usd', 0), total: numOr(v, 'total', 0), api_requests: numOr(v, 'api_requests', 0) };
    case 'exit':
      return { kind: 'exit', code: numOr(v, 'code', 0) };
    case 'raw':
      return { kind: 'raw', text: str(v, 'text'), stream: str(v, 'stream') };
    default:
      return { kind: 'unknown', raw: safeStringify(v) };
  }
}

function safeStringify(v: unknown): string {
  try {
    return JSON.stringify(v);
  } catch {
    return String(v);
  }
}

/// Parse one raw session.jsonl line (the SSE `session` event's data) into a typed [`SessionLine`].
/// Never throws: a parse failure or unmodeled kind returns the `unknown` arm carrying the raw line.
export function parseSessionLine(data: string): SessionLine {
  let raw: unknown;
  try {
    raw = JSON.parse(data);
  } catch {
    return { kind: 'unknown', raw: data };
  }
  if (!isRecord(raw) || typeof raw.kind !== 'string') {
    return { kind: 'unknown', raw: data };
  }
  switch (raw.kind) {
    case 'start':
      return { kind: 'start', goal: str(raw, 'goal'), gate: str(raw, 'gate'), model: str(raw, 'model'), namespace: str(raw, 'namespace'), iters_total: numOr(raw, 'iters_total', 0), max_cost: numOr(raw, 'max_cost', 0), max_secs: numOr(raw, 'max_secs', 0) };
    case 'phase':
      return { kind: 'phase', phase: str(raw, 'phase'), iter: numOr(raw, 'iter', 0) };
    case 'note':
      return { kind: 'note', msg: str(raw, 'msg') };
    case 'row':
      return { kind: 'row', row: parseRow(raw.row), solved: boolOr(raw, 'solved', false) };
    case 'agent_start':
      return { kind: 'agent_start', iter: numOr(raw, 'iter', 0) };
    case 'agent':
      return { kind: 'agent', event: parseAgent(raw.event) };
    case 'agent_done':
      return { kind: 'agent_done' };
    case 'budget':
      return { kind: 'budget', spent: numOr(raw, 'spent', 0), elapsed_secs: numOr(raw, 'elapsed_secs', 0) };
    case 'summary':
      return { kind: 'summary', gate: str(raw, 'gate'), best_score: numOr(raw, 'best_score', 0) };
    case 'escalation':
      return { kind: 'escalation', category: str(raw, 'category'), reason: str(raw, 'reason'), evidence: str(raw, 'evidence') };
    case 'segment':
      return { kind: 'segment', fingerprint: str(raw, 'fingerprint'), baseline_score: numOr(raw, 'baseline_score', 0), regime: str(raw, 'regime') };
    case 'identity':
      return { kind: 'identity', digest: isRecord(raw.identity) ? str(raw.identity, 'digest') : '' };
    case 'finished':
      return { kind: 'finished' };
    case 'shutdown':
      return { kind: 'shutdown', outcome: str(raw, 'outcome'), reason: str(raw, 'reason') };
    default:
      return { kind: 'unknown', raw: data };
  }
}

/// Parse the SSE `status` event's data into a [`LiveStatus`], or `null` if it isn't a status object.
export function parseStatus(data: string): LiveStatus | null {
  let raw: unknown;
  try {
    raw = JSON.parse(data);
  } catch {
    return null;
  }
  if (!isRecord(raw)) return null;
  return {
    phase: str(raw, 'phase'),
    iter: numOr(raw, 'iter', 0),
    best_score: optNum(raw, 'best_score'),
    spend: numOr(raw, 'spend', 0),
    paused: boolOr(raw, 'paused', false),
    max_cost: optNum(raw, 'max_cost'),
  };
}
