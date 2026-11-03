// What a playbook launch's detail page says about where it came from and where its dispatch got
// to. Pure functions so both are testable without a render.

import type { components } from '../api/schema';

type LaunchDetail = components['schemas']['PlaybookLaunchDetailDto'];
type PlaybookRun = components['schemas']['PlaybookRunDto'];
type IssueEvent = components['schemas']['EventDto'];
type DispatchState = components['schemas']['DispatchState'];

/// A launch key addresses itself: `playbook:{pack}:{uuidv7}`. Its detail is the launch view, never
/// the issue journey — a playbook row neither ranks nor scopes.
export function isLaunchKey(key: string): boolean {
  return key.startsWith('playbook:');
}

/** A launch key as a reader names it: its pack and the head of its id. */
export function launchLabel(key: string): string {
  const match = /^playbook:(.+):([0-9a-f-]+)$/i.exec(key);
  const pack = match?.[1];
  const id = match?.[2];
  return pack === undefined || id === undefined ? key : `${pack} ${id.slice(0, 8)}`;
}

/** A launch's run, on the run page nested under the launch. */
export function launchRunPath(key: string, runId: string): string {
  return `${launchPath(key)}/runs/${encodeURIComponent(runId)}`;
}

export function launchPath(key: string): string {
  return `/playbook-runs/${encodeURIComponent(key)}`;
}

/** Where a key's detail page lives: a launch has its own, everything else is an issue. */
export function detailPath(key: string): string {
  return isLaunchKey(key) ? launchPath(key) : `/issues/${encodeURIComponent(key)}`;
}

/// Relaunch: the launch form rendered against the pack's current schema with this run's frozen
/// values in it, editable before it fires. The form reads the snapshot back by key, so what is
/// carried here is the key, not the values.
export function relaunchPath(playbook: string, key: string): string {
  return `/playbooks/${encodeURIComponent(playbook)}/launch?relaunch=${encodeURIComponent(key)}`;
}

export interface OriginLink {
  /** What produced the launch: the surface a reader would go back to. */
  label: string;
  value: string;
  /** The route back, or null when the source is gone or has no surface of its own. */
  to: string | null;
}

const ORIGIN_LABEL: Record<string, string> = {
  manual: 'launch form',
  deferred: 'one-shot',
  schedule: 'schedule',
  draft: 'draft studio',
};

/** How the launch is described in one word: its origin, spelled for a reader. */
export function originLabel(origin: string): string {
  return ORIGIN_LABEL[origin] ?? origin;
}

/** The pack a launch froze: a draft's save number, else the schema digest it was admitted under. */
export function launchVersion(launch: Pick<PlaybookRun, 'draft_version' | 'schema_digest'>): string {
  if (launch.draft_version !== null && launch.draft_version !== undefined) {
    return `draft v${launch.draft_version}`;
  }
  return launch.schema_digest.replace(/^sha256:/, '').slice(0, 12);
}

/// How long a launch ran, in seconds: from its move into running to the move that took it out of
/// running. Null while it still runs, or when its trail holds no such pair.
export function launchWallSecs(events: readonly Pick<IssueEvent, 'from' | 'to' | 'ts'>[]): number | null {
  const trail = [...events].sort((a, b) => Date.parse(a.ts) - Date.parse(b.ts));
  const start = trail.findIndex((e) => e.to === 'running' && e.from !== 'running');
  const started = trail[start];
  if (started === undefined) return null;
  const ended = trail.slice(start + 1).find((e) => e.from === 'running' && e.to !== 'running');
  if (ended === undefined) return null;
  const secs = (Date.parse(ended.ts) - Date.parse(started.ts)) / 1000;
  return Number.isFinite(secs) && secs >= 0 ? secs : null;
}

/// Where a launch came from, as links back. A draft launch points at the studio that fired it, any
/// other at the registry row it pinned; a schedule's firing also names the schedule, which has no
/// surface of its own to link to.
export function originLinks(detail: LaunchDetail): OriginLink[] {
  const { launch, source_exists } = detail;
  const links: OriginLink[] = [];
  if (launch.draft_version === null || launch.draft_version === undefined) {
    links.push({
      label: 'pack',
      value: launch.playbook,
      to: source_exists ? `/playbooks/${encodeURIComponent(launch.playbook)}/launch` : null,
    });
  } else {
    links.push({
      label: 'draft',
      value: `${launch.playbook} v${launch.draft_version}`,
      to: source_exists ? `/playbooks/drafts/${encodeURIComponent(launch.playbook)}` : null,
    });
  }
  if (launch.schedule !== null && launch.schedule !== undefined) {
    links.push({ label: 'schedule', value: launch.schedule, to: null });
  }
  if (detail.rerun) {
    links.push({
      label: 'rerun of',
      value: detail.rerun.run_id,
      to: `/runs/${encodeURIComponent(detail.rerun.run_id)}`,
    });
  }
  return links;
}

export type DispatchTone = 'grey' | 'blue' | 'red';

const DISPATCH_TONE: Record<DispatchState, DispatchTone> = {
  pending: 'grey',
  dispatched: 'blue',
  failed: 'red',
};

export function dispatchTone(state: DispatchState): DispatchTone {
  return DISPATCH_TONE[state];
}

export interface DispatchFailure {
  label: string;
  error: string;
  at: string | null;
  blocking: boolean;
}

/// The stored dispatch failure: what keeps a launch from starting, or, once a run exists, the
/// last attempt that failed before it. Null when no attempt failed.
export function dispatchFailure(dispatch: LaunchDetail['dispatch']): DispatchFailure | null {
  if (!dispatch.failure) return null;
  const blocking = dispatch.state !== 'dispatched';
  return {
    label: blocking ? 'why it did not start' : 'last failed attempt',
    error: dispatch.failure,
    at: dispatch.failed_at ?? null,
    blocking,
  };
}

/// The run whose graph the page draws: the newest one this launch dispatched, since a re-dispatch
/// supersedes what came before. Null when nothing ever started.
export function graphRunId(detail: LaunchDetail): string | null {
  return detail.runs[0]?.run_id ?? null;
}

type RerunPreview = components['schemas']['RerunPreviewDto'];

export type RerunState = 'reused' | 'reruns' | 'changed';

export interface RerunRow {
  task: string;
  state: RerunState;
  /** What a reused task cost the run that settled it; null for a task that runs again. */
  costUsd: number | null;
}

/// A rerun's tasks as the panel lists them: what is reused in the order the parent settled it,
/// then what runs again, then any reused task that changed since the parent.
/** A rerun preview in one line: how many tasks, and how many of each are reused, rerun or changed. */
export function rerunSummary(rows: readonly RerunRow[]): string {
  const tasks = `${rows.length} ${rows.length === 1 ? 'task' : 'tasks'}`;
  const count = (state: RerunState) => rows.filter((row) => row.state === state).length;
  const reused = count('reused');
  if (rows.length > 0 && reused === rows.length) return `${tasks} · all reused`;
  const parts: [number, string][] = [
    [reused, 'reused'],
    [count('reruns'), 'rerun'],
    [count('changed'), 'changed'],
  ];
  return [tasks, ...parts.filter(([n]) => n > 0).map(([n, word]) => `${n} ${word}`)].join(' · ');
}

export function rerunRows(preview: RerunPreview): RerunRow[] {
  return [
    ...preview.reused.map((r) => ({ task: r.task, state: 'reused' as const, costUsd: r.cost_usd })),
    ...preview.rerun.map((task) => ({ task, state: 'reruns' as const, costUsd: null })),
    ...preview.changed.map((task) => ({ task, state: 'changed' as const, costUsd: null })),
  ];
}

/// Add `task` to a rerun's selection, keeping it sorted and free of repeats so the preview query
/// key is stable.
export function withTask(from: readonly string[], task: string): string[] {
  return [...new Set([...from, task])].sort();
}

/// The launch body a rerun posts: the parent launch's own values and ceilings.
export function rerunLaunchBody(
  launch: LaunchDetail['launch'],
  runId: string,
  from: readonly string[],
) {
  return {
    params: Object.fromEntries(
      Object.entries(launch.params).map(([name, value]) => [name, String(value)]),
    ),
    max_cost: launch.max_cost,
    max_time: launch.max_time,
    rerun: { of: runId, from: [...from] },
  };
}
