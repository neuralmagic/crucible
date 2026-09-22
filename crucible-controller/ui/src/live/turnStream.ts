// Pure helpers for the live turn view (GET /api/turns/{pod}/live): parse the SSE `progress`
// payloads the scope engine emits at round boundaries, and cap the client-side log buffer. Kept
// free of React/DOM so turnStream.test.ts drives them directly (the funnel.ts convention).

import { isRecord } from '../json';

/// One `CRUCIBLE_SCOPE_PROGRESS` beat, as `crucible::scope::ScopeProgress` serializes it.
export interface ScopeProgressBeat {
  round: number;
  kind: 'propose' | 'refine' | 'adversary';
  doing: string;
  cost_so_far: number;
}

const PROGRESS_KINDS: ReadonlySet<string> = new Set(['propose', 'refine', 'adversary']);

function isProgressKind(kind: string): kind is ScopeProgressBeat['kind'] {
  return PROGRESS_KINDS.has(kind);
}

/** Decode one SSE `progress` event's data. Null on anything malformed — the stream relays pod
 * logs, so a payload is never trusted to be well-shaped. */
export function parseProgress(data: string): ScopeProgressBeat | null {
  let value: unknown;
  try {
    value = JSON.parse(data);
  } catch {
    return null;
  }
  if (!isRecord(value)) return null;
  const { round, kind, doing, cost_so_far } = value;
  if (typeof round !== 'number' || !Number.isInteger(round) || round < 1) return null;
  if (typeof kind !== 'string' || !isProgressKind(kind)) return null;
  if (typeof doing !== 'string') return null;
  if (typeof cost_so_far !== 'number' || Number.isNaN(cost_so_far)) return null;
  return { round, kind, doing, cost_so_far };
}

/** Fold a beat into the round rail. An EventSource reconnect replays the whole log, so the same
 * (round, kind) beat can arrive twice — it replaces its earlier self instead of duplicating.
 * Returns a new array sorted by round (kind order within a round follows arrival). */
export function upsertBeat(beats: ScopeProgressBeat[], beat: ScopeProgressBeat): ScopeProgressBeat[] {
  const idx = beats.findIndex((b) => b.round === beat.round && b.kind === beat.kind);
  const next = beats.slice();
  if (idx >= 0) {
    next[idx] = beat;
  } else {
    next.push(beat);
  }
  next.sort((a, b) => a.round - b.round);
  return next;
}

/// One `CRUCIBLE_SCOPE_ACTIVITY` beat, as `crucible::scope::ScopeActivity` serializes it: the
/// engine's bounded within-round feed (tool calls, text snippets, usage samples, sandbox stage
/// banners, and a final truncation notice when the byte budget runs out).
export interface ScopeActivityBeat {
  kind: 'tool' | 'text' | 'usage' | 'stage' | 'truncated';
  name?: string;
  detail: string;
  cost_so_far: number;
}

const ACTIVITY_KINDS: ReadonlySet<string> = new Set(['tool', 'text', 'usage', 'stage', 'truncated']);

function isActivityKind(kind: string): kind is ScopeActivityBeat['kind'] {
  return ACTIVITY_KINDS.has(kind);
}

/** Decode one SSE `activity` event's data. Null on anything malformed, same trust posture as
 * parseProgress. */
export function parseActivity(data: string): ScopeActivityBeat | null {
  let value: unknown;
  try {
    value = JSON.parse(data);
  } catch {
    return null;
  }
  if (!isRecord(value)) return null;
  const { kind, name, detail, cost_so_far } = value;
  if (typeof kind !== 'string' || !isActivityKind(kind)) return null;
  if (name !== undefined && typeof name !== 'string') return null;
  if (typeof detail !== 'string') return null;
  if (typeof cost_so_far !== 'number' || Number.isNaN(cost_so_far)) return null;
  return name === undefined ? { kind, detail, cost_so_far } : { kind, name, detail, cost_so_far };
}

/** The ticker's one-line rendering of a beat: what the agent is doing right now. */
export function activityLabel(beat: ScopeActivityBeat): string {
  switch (beat.kind) {
    case 'tool':
      return beat.name ? `${beat.name} · ${beat.detail}` : beat.detail;
    case 'usage':
      return beat.detail;
    case 'stage':
      return beat.detail;
    case 'truncated':
      return beat.detail;
    case 'text':
      return `“${beat.detail}”`;
  }
}

/// One buffered log line; `id` is a monotonic counter, the React key.
export interface LogLine {
  id: number;
  text: string;
}

/** Append one line, dropping the oldest past `cap`. Returns a new array; never mutates. */
export function appendLog(lines: LogLine[], id: number, text: string, cap: number): LogLine[] {
  const next = lines.slice();
  next.push({ id, text });
  return next.length > cap ? next.slice(next.length - cap) : next;
}

export type PhaseColor = 'grey' | 'blue' | 'green' | 'red';

/** Badge color for a pod phase string off the wire. */
export function phaseColor(phase: string): PhaseColor {
  switch (phase) {
    case 'Running':
      return 'blue';
    case 'Succeeded':
      return 'green';
    case 'Failed':
      return 'red';
    default:
      // Pending / Unknown / anything the cluster invents later.
      return 'grey';
  }
}
