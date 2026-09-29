import { describe, expect, it } from 'vitest';
import {
  DEFAULT_FILTERS,
  isDraftRun,
  parseRunFilters,
  runFilterParams,
  runInContext,
  runsView,
  type RunFilters,
  type RunRow,
} from './playbookRunsView';

function run(over: Partial<RunRow> & { id?: string } = {}): RunRow & { id: string } {
  return { id: 'r', playbook: 'survey', status: 'done', origin: 'manual', created_by: 'wren', ...over };
}

const RUNS = [
  run({ id: 'a', status: 'done' }),
  run({ id: 'b', status: 'parked' }),
  run({ id: 'c', status: 'parked', playbook: 'triage' }),
  run({ id: 'd', status: 'parked', origin: 'draft' }),
  run({ id: 'e', status: 'parked', origin: 'draft', playbook: 'triage' }),
  run({ id: 'f', status: 'running', origin: 'deferred', playbook: 'triage' }),
];

function ids(filters: Partial<RunFilters>): string[] {
  return runsView(RUNS, { ...DEFAULT_FILTERS, ...filters }).rows.map((r) => r.id);
}

describe('runsView', () => {
  it('hides draft launches by default and says how many', () => {
    const view = runsView(RUNS, DEFAULT_FILTERS);
    expect(view.rows.map((r) => r.id)).toEqual(['a', 'b', 'c', 'f']);
    expect(view.hiddenDrafts).toBe(2);
  });

  it('shows drafts when asked, with nothing hidden', () => {
    const view = runsView(RUNS, { ...DEFAULT_FILTERS, drafts: true });
    expect(view.rows).toHaveLength(6);
    expect(view.hiddenDrafts).toBe(0);
  });

  it('picking the draft origin shows drafts regardless of the toggle', () => {
    expect(ids({ origin: 'draft' })).toEqual(['d', 'e']);
    expect(runsView(RUNS, { ...DEFAULT_FILTERS, origin: 'draft' }).hiddenDrafts).toBe(0);
  });

  it('combines filters', () => {
    expect(ids({ status: 'parked' })).toEqual(['b', 'c']);
    expect(ids({ status: 'parked', playbook: 'triage' })).toEqual(['c']);
    expect(ids({ status: 'parked', playbook: 'triage', drafts: true })).toEqual(['c', 'e']);
  });

  it('counts only the drafts the other filters would keep as hidden', () => {
    expect(runsView(RUNS, { ...DEFAULT_FILTERS, playbook: 'triage' }).hiddenDrafts).toBe(1);
    expect(runsView(RUNS, { ...DEFAULT_FILTERS, status: 'running' }).hiddenDrafts).toBe(0);
  });

  it('counts each axis against the other filters, busiest first', () => {
    const view = runsView(RUNS, { ...DEFAULT_FILTERS, playbook: 'triage' });
    expect(view.status).toEqual([
      { value: '', label: 'All', count: 2 },
      { value: 'parked', label: 'parked', count: 1 },
      { value: 'running', label: 'running', count: 1 },
    ]);
    expect(view.playbook).toEqual([
      { value: '', label: 'All', count: 4 },
      { value: 'survey', label: 'survey', count: 2 },
      { value: 'triage', label: 'triage', count: 2 },
    ]);
  });

  it('counts hidden drafts on the origin axis so they can be picked', () => {
    const view = runsView(RUNS, DEFAULT_FILTERS);
    expect(view.origin).toEqual([
      { value: '', label: 'All', count: 6 },
      { value: 'manual', label: 'launch form', count: 3 },
      { value: 'draft', label: 'draft studio', count: 2 },
      { value: 'deferred', label: 'one-shot', count: 1 },
    ]);
  });

  it('keeps a selected value with no rows as an option', () => {
    const view = runsView(RUNS, { ...DEFAULT_FILTERS, status: 'done', playbook: 'triage' });
    expect(view.rows).toEqual([]);
    expect(view.playbook.find((o) => o.value === 'triage')).toEqual({ value: 'triage', label: 'triage', count: 0 });
  });
});

describe('filter params', () => {
  it('round-trips through the query string', () => {
    const filters: RunFilters = { status: 'parked', playbook: 'triage', origin: 'manual', drafts: true };
    expect(parseRunFilters(runFilterParams(filters))).toEqual(filters);
  });

  it('leaves defaults out of the query string', () => {
    expect(runFilterParams(DEFAULT_FILTERS).toString()).toBe('');
    expect(parseRunFilters(new URLSearchParams())).toEqual(DEFAULT_FILTERS);
  });

  it('reads anything but drafts=shown as hidden', () => {
    expect(parseRunFilters(new URLSearchParams('drafts=yes')).drafts).toBe(false);
  });
});

describe('runInContext', () => {
  const owners = new Map([
    ['survey', 'team:llm-d'],
    ['triage', 'user:wren'],
  ]);
  const ownerOf = (id: string) => owners.get(id);

  it('keeps everything in the all context', () => {
    expect(runInContext(run({ created_by: null }), 'all', ownerOf)).toBe(true);
  });

  it('keeps what the user launched in their own context, case-insensitively', () => {
    expect(runInContext(run({ created_by: 'Wren' }), 'user:wren', ownerOf)).toBe(true);
    expect(runInContext(run({ created_by: 'kyle' }), 'user:wren', ownerOf)).toBe(false);
    expect(runInContext(run({ created_by: null }), 'user:wren', ownerOf)).toBe(false);
  });

  it('keeps runs of the team playbooks in a team context', () => {
    expect(runInContext(run({ playbook: 'survey', created_by: 'kyle' }), 'team:llm-d', ownerOf)).toBe(true);
    expect(runInContext(run({ playbook: 'triage' }), 'team:llm-d', ownerOf)).toBe(false);
    expect(runInContext(run({ playbook: 'gone' }), 'team:llm-d', ownerOf)).toBe(false);
  });
});

describe('isDraftRun', () => {
  it('is true only for draft studio launches', () => {
    expect(isDraftRun({ origin: 'draft' })).toBe(true);
    expect(isDraftRun({ origin: 'manual' })).toBe(false);
    expect(isDraftRun({ origin: 'deferred' })).toBe(false);
  });
});
