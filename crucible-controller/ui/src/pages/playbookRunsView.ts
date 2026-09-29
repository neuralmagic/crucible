import type { components } from '../api/schema';
import { ALL } from '../ownerContext';
import type { FacetOption } from '../ui';
import { originLabel } from './launchView';

type PlaybookRunDto = components['schemas']['PlaybookRunDto'];

export type RunRow = Pick<PlaybookRunDto, 'playbook' | 'status' | 'origin' | 'created_by'>;

export interface RunFilters {
  status: string;
  playbook: string;
  origin: string;
  /// Launches fired from the draft studio are test runs, hidden unless asked for.
  drafts: boolean;
}

export const DEFAULT_FILTERS: RunFilters = { status: '', playbook: '', origin: '', drafts: false };

const DRAFT = 'draft';

/// A launch fired from the draft studio: a test run.
export function isDraftRun(run: Pick<RunRow, 'origin'>): boolean {
  return run.origin === DRAFT;
}

export function parseRunFilters(params: URLSearchParams): RunFilters {
  return {
    status: params.get('status') ?? '',
    playbook: params.get('playbook') ?? '',
    origin: params.get('origin') ?? '',
    drafts: params.get('drafts') === 'shown',
  };
}

/// The query string for `filters`, defaults left out so the bare URL is the default view.
export function runFilterParams(filters: RunFilters): URLSearchParams {
  const params = new URLSearchParams();
  if (filters.status) params.set('status', filters.status);
  if (filters.playbook) params.set('playbook', filters.playbook);
  if (filters.origin) params.set('origin', filters.origin);
  if (filters.drafts) params.set('drafts', 'shown');
  return params;
}

/// Whether a run belongs to the owner context. A launch carries no owner: in a user's context it is
/// theirs when they launched it, in a team's when the team owns the playbook it ran.
export function runInContext(
  run: RunRow,
  context: string,
  playbookOwner: (playbook: string) => string | undefined,
): boolean {
  if (context === ALL) return true;
  if (context.startsWith('user:')) return (run.created_by ?? '').toLowerCase() === context.slice('user:'.length);
  return playbookOwner(run.playbook) === context;
}

type Axis = 'status' | 'playbook' | 'origin';

function passes(run: RunRow, filters: RunFilters, skip: Axis | null): boolean {
  const draftsVisible = filters.drafts || filters.origin === DRAFT || skip === 'origin';
  if (isDraftRun(run) && !draftsVisible) return false;
  if (skip !== 'status' && filters.status && run.status !== filters.status) return false;
  if (skip !== 'playbook' && filters.playbook && run.playbook !== filters.playbook) return false;
  if (skip !== 'origin' && filters.origin && run.origin !== filters.origin) return false;
  return true;
}

function options(
  rows: readonly RunRow[],
  axis: Axis,
  selected: string,
  label: (value: string) => string,
): FacetOption[] {
  const counts = new Map<string, number>();
  for (const row of rows) counts.set(row[axis], (counts.get(row[axis]) ?? 0) + 1);
  if (selected && !counts.has(selected)) counts.set(selected, 0);
  const values = [...counts.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
  return [
    { value: '', label: 'All', count: rows.length },
    ...values.map(([value, count]) => ({ value, label: label(value), count })),
  ];
}

export interface RunsView<T extends RunRow> {
  rows: T[];
  status: FacetOption[];
  playbook: FacetOption[];
  origin: FacetOption[];
  /// Draft launches the current filters leave out only because drafts are hidden.
  hiddenDrafts: number;
}

/// The rows the filters keep, and each axis's options counted against every other filter, so a
/// count is what picking that option would show.
export function runsView<T extends RunRow>(runs: readonly T[], filters: RunFilters): RunsView<T> {
  const along = (axis: Axis) => runs.filter((run) => passes(run, filters, axis));
  const rows = runs.filter((run) => passes(run, filters, null));
  const shown = { ...filters, drafts: true };
  const hiddenDrafts = runs.filter((run) => isDraftRun(run) && passes(run, shown, null)).length -
    rows.filter(isDraftRun).length;
  return {
    rows,
    status: options(along('status'), 'status', filters.status, (v) => v),
    playbook: options(along('playbook'), 'playbook', filters.playbook, (v) => v),
    origin: options(along('origin'), 'origin', filters.origin, originLabel),
    hiddenDrafts,
  };
}
