// What a playbook launch's detail page says about where it came from and where its dispatch got
// to. Pure functions so both are testable without a render.

import type { components } from '../api/schema';

type LaunchDetail = components['schemas']['PlaybookLaunchDetailDto'];
type DispatchState = components['schemas']['DispatchState'];

/// A launch key addresses itself: `playbook:{pack}:{uuidv7}`. Its detail is the launch view, never
/// the issue journey — a playbook row neither ranks nor scopes.
export function isLaunchKey(key: string): boolean {
  return key.startsWith('playbook:');
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

/// The run whose graph the page draws: the newest one this launch dispatched, since a re-dispatch
/// supersedes what came before. Null when nothing ever started.
export function graphRunId(detail: LaunchDetail): string | null {
  return detail.runs[0]?.run_id ?? null;
}
