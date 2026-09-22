// What a task card and its metadata panel say, derived from one `GraphNodeDto`. Pure functions,
// kept out of the renderer so the mapping is testable without a flow.

import { formatCost } from './runReport';
import type { TaskRuntime } from './taskGraph';
import type { WorkflowGraphNode } from './workflowGraphLayout';

/// A field the wire may carry as absent, null, or a string.
export function said(value: string | null | undefined): value is string {
  return typeof value === 'string' && value.length > 0;
}

export function badgeFor(node: WorkflowGraphNode): string {
  if (node.kind === 'agent') return 'AGENT';
  if (node.kind === 'command') return 'CMD';
  if (node.kind === 'engine') return 'ENGINE';
  return 'TASK';
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

/// What a task runs, in one line: an agent's knobs, or the command line itself.
export function runsLine(node: WorkflowGraphNode): string | null {
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

/// A run's node state, as the card's status line reads it: where the task ended, how many attempts
/// it took, and what those attempts cost.
export function runtimeLine(runtime: TaskRuntime): string {
  const parts = [`iter ${runtime.latestIter}`];
  if (runtime.attempts > 1) parts.push(`${runtime.attempts} attempts`);
  if (runtime.costUsd !== null) parts.push(formatCost(runtime.costUsd));
  if (runtime.secs !== null) parts.push(formatSecs(runtime.secs));
  return parts.join(' · ');
}

export function formatSecs(secs: number): string {
  if (secs < 60) return `${Math.round(secs)}s`;
  const whole = Math.round(secs);
  return `${Math.floor(whole / 60)}m ${whole % 60}s`;
}

/// The run's own rows, listed above the scheduling ones a plan task shares with a compiled pack.
export function runtimeRows(runtime: TaskRuntime): DetailRow[] {
  return [
    { label: 'status', value: runtime.status },
    { label: 'iteration', value: String(runtime.latestIter) },
    { label: 'attempts', value: String(runtime.attempts) },
    { label: 'cost', value: formatCost(runtime.costUsd) },
    { label: 'took', value: runtime.secs === null ? '—' : formatSecs(runtime.secs) },
  ];
}
