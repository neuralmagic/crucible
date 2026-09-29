import type { components } from '../api/schema';
import { budgetState, resetsIn, type SharedBudget } from '../budget';
import { launchPath } from './launchView';
import { scheduleView, type ScheduleDto } from './schedulesView';

type PlaybookRunDto = components['schemas']['PlaybookRunDto'];
type PlaybookDto = components['schemas']['PlaybookDto'];
type ApprovalsDto = components['schemas']['ApprovalsDto'];

export type RunRow = Pick<PlaybookRunDto, 'key' | 'playbook' | 'status' | 'parked_reason' | 'created_at'>;
export type PlaybookRow = Pick<PlaybookDto, 'id' | 'updated_at'>;
export type ScheduleRow = Pick<
  ScheduleDto,
  | 'id'
  | 'playbook'
  | 'enabled'
  | 'next_due_at'
  | 'consecutive_failures'
  | 'owner_signin_required'
  | 'owner_refresh_error'
  | 'owner_refresh_at'
>;

/// Parked launches listed one by one before the rest collapse into a count.
export const PARKED_SHOWN = 3;

export interface AttentionItem {
  key: string;
  tone: 'red' | 'amber';
  text: string;
  to: string;
}

function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

/// What is waiting on a person, most urgent first. Empty when nothing is.
export function attentionItems(
  runs: readonly RunRow[],
  schedules: readonly ScheduleRow[],
  waitingApproval: number,
): AttentionItem[] {
  const items: AttentionItem[] = [];

  const parked = runs.filter((run) => run.status === 'parked');
  for (const run of parked.slice(0, PARKED_SHOWN)) {
    items.push({
      key: `parked:${run.key}`,
      tone: 'amber',
      text: `${run.playbook} is parked: ${run.parked_reason ?? 'no reason recorded'}`,
      to: launchPath(run.key),
    });
  }
  const unshown = parked.length - PARKED_SHOWN;
  if (unshown > 0) {
    items.push({
      key: 'parked:more',
      tone: 'amber',
      text: plural(unshown, 'more parked run', 'more parked runs'),
      to: '/playbook-runs',
    });
  }

  const states = schedules.map((row) => ({ row, state: scheduleView(row).state }));
  const signin = states.filter((s) => s.state === 'signin').length;
  if (signin > 0) {
    items.push({
      key: 'schedules:signin',
      tone: 'red',
      text:
        signin === 1
          ? '1 schedule stops firing until its owner signs in again'
          : `${signin} schedules stop firing until their owners sign in again`,
      to: '/schedules',
    });
  }
  const stopped = states.filter((s) => s.state === 'disabled' && s.row.consecutive_failures > 0).length;
  if (stopped > 0) {
    items.push({
      key: 'schedules:stopped',
      tone: 'amber',
      text: `${plural(stopped, 'schedule was', 'schedules were')} disabled after failing to launch`,
      to: '/schedules',
    });
  }

  if (waitingApproval > 0) {
    items.push({
      key: 'approvals',
      tone: 'amber',
      text: `${plural(waitingApproval, 'item is', 'items are')} waiting for approval`,
      to: '/approvals',
    });
  }

  items.sort((a, b) => (a.tone === b.tone ? 0 : a.tone === 'red' ? -1 : 1));
  return items;
}

/// The item a spent shared budget raises: every new launch waits for the UTC day to roll over.
export function budgetAttention(budget: SharedBudget, now: Date): AttentionItem | null {
  if (budgetState(budget) !== 'spent') return null;
  return {
    key: 'budget',
    tone: 'red',
    text: `Shared budget spent. Launches resume in ${resetsIn(now)}.`,
    to: '/playbook-runs',
  };
}

/// Everything the approvals queue holds.
export function approvalsWaiting(approvals: ApprovalsDto): number {
  return approvals.awaiting_approval.length + approvals.pending_imports.length + approvals.kept_prs.length;
}

/// Playbooks most recently launched first, then the rest by most recently updated.
export function playbooksByUse<T extends PlaybookRow>(
  playbooks: readonly T[],
  runs: readonly RunRow[],
  limit: number,
): T[] {
  const lastLaunch = new Map<string, string>();
  for (const run of runs) {
    const seen = lastLaunch.get(run.playbook);
    if (seen === undefined || run.created_at > seen) lastLaunch.set(run.playbook, run.created_at);
  }
  return [...playbooks]
    .sort((a, b) => {
      const la = lastLaunch.get(a.id);
      const lb = lastLaunch.get(b.id);
      if (la !== undefined && lb !== undefined) return lb.localeCompare(la);
      if (la !== undefined) return -1;
      if (lb !== undefined) return 1;
      return b.updated_at.localeCompare(a.updated_at);
    })
    .slice(0, limit);
}

/// Schedules that will fire, soonest first.
export function upcoming<T extends ScheduleRow>(schedules: readonly T[], limit: number): T[] {
  return schedules
    .filter((row) => row.next_due_at && scheduleView(row).state !== 'signin' && row.enabled)
    .sort((a, b) => (a.next_due_at ?? '').localeCompare(b.next_due_at ?? ''))
    .slice(0, limit);
}

/// A future stamp relative to `now`: "in 5m", "in 3h", "in 2d"; "now" once it has passed.
export function untilTime(iso: string | null | undefined, now: number): string | null {
  if (!iso) return null;
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return null;
  const min = Math.floor((then - now) / 60_000);
  if (min < 1) return 'now';
  if (min < 60) return `in ${min}m`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `in ${hr}h`;
  return `in ${Math.floor(hr / 24)}d`;
}
