// Pure feed helpers: fold session lines into display rows (coalescing streamed agent text), and the
// scroll math that drives auto-follow. Kept free of React so they stay unit-testable.

import { parseSessionLine, type AgentEvent, type SessionLine } from './session';

/// One rendered row. Streamed assistant `text` and `thinking` deltas are merged into a single
/// growing block (`text`/`thinking`); every other line is its own `line` row. `id` is the React key:
/// the position of the row's first line in its feed, counted by whoever appends.
export type FeedRow =
  | { id: number; kind: 'text'; text: string }
  | { id: number; kind: 'thinking'; text: string }
  | { id: number; kind: 'line'; line: SessionLine }
  | { id: number; kind: 'pod'; text: string };

/// Append one parsed line to the row buffer, coalescing consecutive agent text/thinking deltas into
/// the trailing block, and capping the buffer to `cap` rows (oldest dropped). Returns a new array;
/// never mutates `rows`.
export function appendLine(rows: FeedRow[], id: number, line: SessionLine, cap: number): FeedRow[] {
  const next = rows.slice();
  pushLine(next, id, line);
  return next.length > cap ? next.slice(next.length - cap) : next;
}

/// The shared coalescing core: append `line` to `rows` IN PLACE (a delta replaces the trailing
/// block with a fresh object, so a caller holding the old element never observes mutation).
/// appendLine wraps this immutably for React state; foldSessionText loops it for one-pass replay.
function pushLine(rows: FeedRow[], id: number, line: SessionLine): void {
  const last = rows[rows.length - 1];

  if (line.kind === 'agent' && line.event.kind === 'text') {
    if (last && last.kind === 'text') {
      rows[rows.length - 1] = { id: last.id, kind: 'text', text: last.text + line.event.delta };
    } else {
      rows.push({ id, kind: 'text', text: line.event.delta });
    }
  } else if (line.kind === 'agent' && line.event.kind === 'thinking') {
    if (last && last.kind === 'thinking') {
      rows[rows.length - 1] = { id: last.id, kind: 'thinking', text: last.text + line.event.delta };
    } else {
      rows.push({ id, kind: 'thinking', text: line.event.delta });
    }
  } else {
    rows.push({ id, kind: 'line', line });
  }
}

/// Append one pod log line (the spoke relay's `log` event, which carries no session structure).
/// Consecutive lines stay separate rows: pod output is line-oriented, unlike streamed agent text.
export function appendPodLine(rows: FeedRow[], id: number, text: string, cap: number): FeedRow[] {
  const next = rows.slice();
  next.push({ id, kind: 'pod', text });
  return next.length > cap ? next.slice(next.length - cap) : next;
}

/// Fold a whole session NDJSON text (a post-facto transcript) into display rows in one pass — the
/// replay sibling of appendLine, without the per-append array copies live streaming pays for.
/// Blank lines are skipped; anything unparseable surfaces as an `unknown` row (parseSessionLine
/// never throws). Over `cap`, the oldest rows drop, same as the live feed.
export function foldSessionText(text: string, cap: number): FeedRow[] {
  const rows: FeedRow[] = [];
  let id = 0;
  for (const raw of text.split('\n')) {
    const line = raw.trim();
    if (line === '') continue;
    pushLine(rows, id, parseSessionLine(line));
    id += 1;
    if (rows.length > cap) rows.splice(0, rows.length - cap);
  }
  return rows;
}

/// Whether a scroll position is within `threshold` px of the bottom — the "still following" test.
export function atBottom(scrollTop: number, scrollHeight: number, clientHeight: number, threshold = 40): boolean {
  return scrollHeight - scrollTop - clientHeight <= threshold;
}

/// The semantic tone a line renders with (drives the accent color).
export type LineTone = 'keep' | 'drop' | 'info' | 'budget' | 'error' | 'success' | 'muted';

/// A flattened, render-ready view of a non-text session line.
export interface LineView {
  tone: LineTone;
  tag: string;
  title: string;
  detail?: string;
  score?: number;
}

/// Classify a candidate `decision` string (`baseline`, `keep`, `reject`, `wide-keep-2`,
/// `wide-drop-1`, …) into a tone.
function decisionTone(decision: string): LineTone {
  const d = decision.toLowerCase();
  if (d.includes('keep')) return 'keep';
  if (d.includes('drop') || d.includes('reject')) return 'drop';
  return 'info';
}

/// Map one [`SessionLine`] to its display view. Text/thinking lines are coalesced before they reach
/// here, but the arms below still handle them defensively (an isolated delta renders as text).
export function describeLine(line: SessionLine): LineView {
  switch (line.kind) {
    case 'start':
      return { tone: 'info', tag: 'start', title: line.goal || 'run started', detail: `gate ${line.gate} · ${line.iters_total} iters · $${line.max_cost.toFixed(2)} cap` };
    case 'phase':
      return { tone: 'info', tag: 'phase', title: line.phase === 'baseline' ? 'baseline' : `iteration ${line.iter}` };
    case 'note':
      return { tone: 'muted', tag: 'note', title: line.msg };
    case 'row': {
      const lane = line.row.phase === 'wide' ? `lane ${line.row.iter}` : `iter ${line.row.iter}`;
      return {
        tone: decisionTone(line.row.decision),
        tag: line.row.phase === 'wide' ? 'wide' : 'candidate',
        title: `${line.row.decision} · ${lane}`,
        detail: line.row.note || undefined,
        score: line.row.score,
      };
    }
    case 'agent_start':
      return { tone: 'muted', tag: 'agent', title: `agent turn · iter ${line.iter}` };
    case 'agent':
      return describeAgent(line.event);
    case 'agent_done':
      return { tone: 'muted', tag: 'agent', title: 'agent turn complete' };
    case 'budget':
      return { tone: 'budget', tag: 'budget', title: `spend $${line.spent.toFixed(2)}`, detail: `${Math.round(line.elapsed_secs)}s elapsed` };
    case 'summary':
      return { tone: 'success', tag: 'summary', title: `best score ${line.best_score.toFixed(1)}`, detail: `gate ${line.gate}` };
    case 'escalation':
      return { tone: 'error', tag: 'escalation', title: line.reason, detail: line.category };
    case 'segment':
      return { tone: 'info', tag: 'segment', title: `re-scoped · ${line.regime}`, detail: `baseline ${line.baseline_score.toFixed(1)}` };
    case 'identity':
      return { tone: 'muted', tag: 'identity', title: line.digest || 'run identity' };
    case 'finished':
      return { tone: 'success', tag: 'finished', title: 'run finished' };
    case 'shutdown':
      return { tone: line.outcome === 'error' ? 'error' : 'info', tag: 'shutdown', title: line.outcome, detail: line.reason || undefined };
    case 'unknown':
      return { tone: 'muted', tag: 'raw', title: line.raw };
  }
}

function describeAgent(ev: AgentEvent): LineView {
  switch (ev.kind) {
    case 'meta':
      return { tone: 'muted', tag: 'agent', title: `model ${ev.model}` };
    case 'log':
      return { tone: 'muted', tag: ev.label || 'log', title: ev.value ?? ev.label };
    case 'gate':
      return { tone: ev.status === 'fail' ? 'error' : 'muted', tag: `gate:${ev.phase}`, title: `${ev.name} ${ev.status}`, detail: ev.error ?? undefined };
    case 'init':
      return { tone: 'muted', tag: 'agent', title: `session init · ${ev.model}`, detail: `${ev.tools} tools · ${ev.agents} agents` };
    case 'thinking':
      return { tone: 'muted', tag: 'thinking', title: ev.delta };
    case 'text':
      return { tone: 'info', tag: 'text', title: ev.delta };
    case 'tool':
      return { tone: 'info', tag: ev.subagent ? 'subagent' : 'tool', title: ev.name, detail: ev.summary || undefined };
    case 'tokens':
      return { tone: 'muted', tag: 'tokens', title: 'token sample' };
    case 'retry':
      return { tone: 'budget', tag: 'retry', title: `attempt ${ev.attempt}/${ev.max}`, detail: ev.error || undefined };
    case 'error':
      return { tone: 'error', tag: 'error', title: ev.message || ev.error_type, detail: ev.error_type || undefined };
    case 'result':
      return { tone: 'success', tag: 'result', title: `${ev.subtype} · ${ev.turns} turns`, detail: `$${ev.cost_usd.toFixed(2)}` };
    case 'otel_summary':
      return { tone: 'info', tag: 'otel', title: `$${ev.cost_usd.toFixed(2)} · ${ev.total} tok`, detail: `${ev.api_requests} api reqs` };
    case 'exit':
      return { tone: ev.code === 0 ? 'muted' : 'error', tag: 'exit', title: `exit ${ev.code}` };
    case 'raw':
      return { tone: 'muted', tag: ev.stream, title: ev.text };
    case 'unknown':
      return { tone: 'muted', tag: 'agent', title: ev.raw };
  }
}

/// Coarse filter buckets for the feed toolbar. `agent` = the model's own output stream, `candidates`
/// = scored candidate rows, `lifecycle` = everything the engine says about the run itself.
export type FeedCategory = 'agent' | 'candidates' | 'lifecycle';

export const ALL_CATEGORIES: FeedCategory[] = ['agent', 'candidates', 'lifecycle'];

export function rowCategory(row: FeedRow): FeedCategory {
  if (row.kind === 'text' || row.kind === 'thinking') return 'agent';
  // Pod output is what the run itself is doing, not what the agent said.
  if (row.kind === 'pod') return 'lifecycle';
  switch (row.line.kind) {
    case 'agent':
    case 'agent_start':
    case 'agent_done':
      return 'agent';
    case 'row':
      return 'candidates';
    default:
      return 'lifecycle';
  }
}

/// Case-insensitive substring match over everything a row renders (text, tag, title, detail), so
/// the search box behaves like eyeballing the feed.
export function rowMatches(row: FeedRow, query: string): boolean {
  const q = query.trim().toLowerCase();
  if (q === '') return true;
  if (row.kind === 'text' || row.kind === 'thinking' || row.kind === 'pod') {
    return row.text.toLowerCase().includes(q);
  }
  const view = describeLine(row.line);
  return [view.tag, view.title, view.detail ?? ''].some((s) => s.toLowerCase().includes(q));
}

/// One point on the live score sparkline: a measured candidate's score, flagged kept/dropped.
export interface ScorePoint {
  id: number;
  score: number;
  kept: boolean;
}

/// Extract the sparkline points from the feed: every candidate row that carried a score, in feed
/// order. Baselines count as kept (they seed the trajectory).
export function scorePoints(rows: FeedRow[]): ScorePoint[] {
  const points: ScorePoint[] = [];
  for (const row of rows) {
    if (row.kind !== 'line' || row.line.kind !== 'row') continue;
    const r = row.line.row;
    if (r.score === undefined || r.score === null) continue;
    points.push({ id: row.id, score: r.score, kept: decisionTone(r.decision) !== 'drop' });
  }
  return points;
}
