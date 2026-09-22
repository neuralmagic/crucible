import type { MonoTone } from '../ui';

// The issue-status wire vocabulary — the exact kebab-case spellings `Status::parse` accepts
// (crucible-controller/src/model.rs). `?status=` filter values and status Labels both key off
// these; an underscored spelling (`awaiting_approval`, `pr_open`) 400s the API.
export const ISSUE_STATUSES = [
  'new',
  'scoped',
  'awaiting-approval',
  'building',
  'running',
  'pr-open',
  'parked',
  'done',
] as const;

export type IssueStatus = (typeof ISSUE_STATUSES)[number];

export type IssueStatusColor = 'blue' | 'green' | 'orange' | 'grey';

export const ISSUE_STATUS_COLORS: Record<IssueStatus, IssueStatusColor> = {
  new: 'grey',
  scoped: 'blue',
  'awaiting-approval': 'orange',
  building: 'blue',
  running: 'blue',
  'pr-open': 'green',
  parked: 'orange',
  done: 'green',
};

const STATUS_SET: ReadonlySet<string> = new Set(ISSUE_STATUSES);

function isIssueStatus(status: string): status is IssueStatus {
  return STATUS_SET.has(status);
}

/** Label color for a status string off the wire; grey for anything outside the vocabulary. */
export function issueStatusColor(status: string): IssueStatusColor {
  return isIssueStatus(status) ? ISSUE_STATUS_COLORS[status] : 'grey';
}

export function tierTone(tier: string): MonoTone {
  if (tier === 'T0') return 'red';
  if (tier === 'T1') return 'amber';
  return 'ink-2';
}
