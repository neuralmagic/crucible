// What a task card and its metadata panel say, derived from one `GraphNodeDto`. Pure functions,
// kept out of the renderer so the mapping is testable without a flow.

import { formatCost } from './runReport';
import type { ExternalLinkRef } from '../ui';
import type { FanOutState, NodeLinks, RouteDecision, TaskRuntime } from './taskGraph';
import type { WorkflowGraphNode } from './workflowGraphLayout';

/// A field the wire may carry as absent, null, or a string.
export function said(value: string | null | undefined): value is string {
  return typeof value === 'string' && value.length > 0;
}

/// A mapped task is a fan-out over a producer's field, not the work itself: the agent or command
/// is what each of its instances runs, and the instances carry that badge. A route decides however
/// many times it runs, and says so either way.
export function badgeFor(node: WorkflowGraphNode): string {
  if (node.kind === 'route') return 'DECIDE';
  if ((node.fanout ?? null) !== null) return 'MAP';
  if (node.kind === 'agent') return 'AGENT';
  if (node.kind === 'command') return 'CMD';
  if (node.kind === 'engine') return 'ENGINE';
  return 'TASK';
}

/// The glyph a card shows for what runs the task; `badgeFor` is its name.
export function kindGlyph(node: WorkflowGraphNode): string {
  if (node.kind === 'route') return '◇';
  if ((node.fanout ?? null) !== null) return '≡';
  if (node.kind === 'agent') return '✦';
  if (node.kind === 'command') return '›';
  if (node.kind === 'engine') return '⚙';
  return '·';
}

/// How a node's status dot reads: passed, failed, never dispatched because something upstream
/// stopped it, not reached yet in a run still going, or ended some other way (skipped, not taken,
/// never ran).
export type StatusMark = 'pass' | 'fail' | 'blocked' | 'active' | 'idle';

export function statusMark(runtime: TaskRuntime): StatusMark {
  if (runtime.tone === 'pass') return 'pass';
  if (runtime.tone === 'fail') return 'fail';
  if (runtime.status === 'blocked') return 'blocked';
  return runtime.status === 'pending' ? 'active' : 'idle';
}

/// One line of secondary text, or none. A mapped task says what it maps over, since that is what
/// makes it many tasks; otherwise where it runs. What it emits is drawn as chips beside it.
export function metaFor(node: WorkflowGraphNode): string | null {
  const fanout = node.fanout ?? null;
  if (fanout !== null) {
    const cap = typeof fanout.max_fanout === 'number' ? ` ≤${fanout.max_fanout}` : '';
    return `over ${fanout.over_task}.${fanout.over_field}${cap}`;
  }
  if (typeof node.isolation === 'string') return `in ${node.isolation}`;
  if (said(node.session)) return `⟳ ${node.session}`;
  return null;
}

/// What a task runs, in one line: an agent's knobs, the command line itself, or the questions a
/// route asks.
export function runsLine(node: WorkflowGraphNode): string | null {
  if (node.kind === 'route') {
    return node.questions.length === 0 ? null : node.questions.map((q) => `${q}?`).join(' ');
  }
  if (node.kind === 'agent') {
    const knobs = [node.model, node.effort].filter(said);
    return knobs.length === 0 ? 'pack defaults' : knobs.join(' · ');
  }
  return said(node.command) ? node.command : null;
}

export interface DetailRow {
  label: string;
  value: string;
}

/// The labels one question resolved to, most frequent first. A single decision reads as its label
/// alone; a tally of a mapped route's instances carries each label's count.
export function decisionText(decision: RouteDecision): string {
  const labels = [...decision.labels].sort(
    (a, b) => b.count - a.count || a.label.localeCompare(b.label)
  );
  const [only, ...rest] = labels;
  if (only !== undefined && rest.length === 0 && only.count === 1) return only.label;
  return labels.map((l) => `${l.label} ${l.count}`).join(' · ');
}

/// One line per question a route resolved: `tier: high`, or `tier: high 80 · low 38`.
export function decisionLines(decisions: RouteDecision[]): string[] {
  return decisions.map((d) => `${d.question}: ${decisionText(d)}`);
}

export function decisionRows(decisions: RouteDecision[]): DetailRow[] {
  return decisions.map((d) => ({ label: d.question, value: decisionText(d) }));
}

/// Everything the graph document holds about one task, as the panel lists it.
export function detailRows(node: WorkflowGraphNode): DetailRow[] {
  const rows: DetailRow[] = [
    { label: 'kind', value: node.kind },
    { label: 'required', value: node.required ? 'yes' : 'no — advisory' },
    { label: 'needs', value: node.needs },
    { label: 'join', value: node.join },
    { label: 'isolation', value: said(node.isolation) ? node.isolation : 'shared workspace' },
  ];
  if (said(node.session)) rows.push({ label: 'session', value: node.session });
  if (said(node.harness)) rows.push({ label: 'harness', value: node.harness });
  if (said(node.model)) rows.push({ label: 'model', value: node.model });
  if (said(node.effort)) rows.push({ label: 'effort', value: node.effort });
  const fanout = node.fanout ?? null;
  if (fanout !== null) {
    rows.push({ label: 'maps over', value: `${fanout.over_task}.${fanout.over_field}` });
    rows.push({
      label: 'max instances',
      value: typeof fanout.max_fanout === 'number' ? String(fanout.max_fanout) : 'uncapped',
    });
  }
  if (node.questions.length > 0) rows.push({ label: 'questions', value: node.questions.join(' ') });
  const when = node.when ?? null;
  if (when !== null) {
    rows.push({
      label: 'when',
      value: `${when.route}.${when.question} in ${when.labels.join('|')}`,
    });
  }
  if (node.keyed.length > 0) rows.push({ label: 'narrows', value: node.keyed.join(' ') });
  if (node.emits.length > 0) rows.push({ label: 'emits', value: node.emits.join(' ') });
  if (node.emits_files.length > 0) {
    rows.push({ label: 'emits files', value: node.emits_files.join(' ') });
  }
  return rows;
}

/// The source a task runs, when the document carries one: an agent's prompt, else the command.
export function sourceFor(node: WorkflowGraphNode): { label: string; body: string } | null {
  if (said(node.prompt)) return { label: 'prompt', body: node.prompt };
  if (said(node.command)) return { label: 'runs', body: node.command };
  return null;
}

/// A run's node state, as the card's status line reads it: the iteration it ended in when that was
/// a retry, how many attempts it took, and what those attempts cost and took. A zero says nothing,
/// so it is left out, and a task with nothing to say has no line.
export function runtimeLine(runtime: TaskRuntime): string | null {
  if (!runtime.reported) return null;
  const parts: string[] = [];
  if (runtime.latestIter > 0) parts.push(`iter ${runtime.latestIter}`);
  if (runtime.attempts > 1) parts.push(`${runtime.attempts} attempts`);
  if (runtime.costUsd !== null && runtime.costUsd > 0) parts.push(formatCost(runtime.costUsd));
  if (runtime.secs !== null && runtime.secs > 0) parts.push(formatSecs(runtime.secs));
  return parts.length === 0 ? null : parts.join(' · ');
}

/// The width to read a fan-out against, or null when there is none to read it against. A run
/// that started more instances than the last fan-out asked for (one task refanned across
/// iterations) has no single width, and claiming one would print "5 of 3 passed".
function widthOf(state: FanOutState): number | null {
  if (state.items === null || state.started > state.items) return null;
  return state.items;
}

/// How an instance status reads in a tally.
function statusWord(status: string): string {
  if (status === 'fail') return 'failed';
  return status.replaceAll('_', ' ');
}

/// What a run made of a mapped task: how wide it was spread against how the instances that
/// started ended, not-taken instances apart from failed ones. A fan-out over nothing says so —
/// zero work asked for is a result, not an absence.
export function fanoutLine(state: FanOutState): string {
  if (state.items === 0 && state.started === 0) return '0 items';
  const width = widthOf(state);
  const parts = [width === null ? `${state.started} started` : String(width)];
  parts.push(`${state.passed} passed`);
  if (state.notTaken > 0) parts.push(`${state.notTaken} not taken`);
  for (const other of state.other) parts.push(`${other.count} ${statusWord(other.status)}`);
  if (width !== null && state.started < width) {
    parts.push(`${width - state.started} ${state.running ? 'pending' : 'never started'}`);
  }
  return parts.join(' · ');
}

/// How many instances a mapped task came to: the items it was spread over, or the instances that
/// started when the run cannot say how many items there were.
export function fanoutCount(state: FanOutState): number {
  return state.items ?? state.started;
}

/// The fan-out's own rows in the task panel, listed above the run's.
export function fanoutRows(state: FanOutState): DetailRow[] {
  return [
    { label: 'items', value: state.items === null ? '—' : String(state.items) },
    { label: 'started', value: String(state.started) },
    { label: 'passed', value: String(state.passed) },
    ...(state.notTaken > 0 ? [{ label: 'not taken', value: String(state.notTaken) }] : []),
    ...state.other.map((other) => ({ label: other.status, value: String(other.count) })),
  ];
}

export function formatSecs(secs: number): string {
  if (secs < 60) return `${Math.round(secs)}s`;
  const whole = Math.round(secs);
  return `${Math.floor(whole / 60)}m ${whole % 60}s`;
}

/// The run's own rows, listed above the scheduling ones a plan task shares with a compiled pack.
export function runtimeRows(runtime: TaskRuntime): DetailRow[] {
  if (!runtime.reported) return [{ label: 'status', value: runtime.status }];
  return [
    { label: 'status', value: runtime.status },
    { label: 'iteration', value: String(runtime.latestIter) },
    { label: 'attempts', value: String(runtime.attempts) },
    { label: 'cost', value: formatCost(runtime.costUsd) },
    { label: 'took', value: runtime.secs === null ? '—' : formatSecs(runtime.secs) },
  ];
}

/// The chip drawn over a pill's end: its one link as it is, several as a count under one mark (the
/// provider's, when they share one). The links themselves are on the run's header.
export type PillChip =
  | { kind: 'link'; link: ExternalLinkRef }
  | { kind: 'count'; provider: string; count: number };

export function pillChip(drawn: NodeLinks | null): PillChip | null {
  if (drawn === null || drawn.kind !== 'links') return null;
  const [first, second] = drawn.links;
  if (first === undefined) return null;
  if (second === undefined) return { kind: 'link', link: first };
  const shared = drawn.links.every((link) => link.provider === first.provider);
  return { kind: 'count', provider: shared ? first.provider : '', count: drawn.links.length };
}

/// The room a pill chip takes, in characters of the pill's note: its text plus the mark and gaps.
export function pillChipChars(chip: PillChip | null): number {
  if (chip === null) return 0;
  return (chip.kind === 'link' ? chip.link.label.length : String(chip.count).length) + 3;
}
