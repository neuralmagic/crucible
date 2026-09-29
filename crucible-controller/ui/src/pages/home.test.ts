import { describe, expect, it } from 'vitest';
import {
  attentionItems,
  budgetAttention,
  PARKED_SHOWN,
  playbooksByUse,
  untilTime,
  upcoming,
  type RunRow,
  type ScheduleRow,
} from './home';

function run(over: Partial<RunRow> = {}): RunRow {
  return {
    key: 'playbook:survey:0001',
    playbook: 'survey',
    status: 'done',
    parked_reason: null,
    created_at: '2026-09-01T00:00:00Z',
    ...over,
  };
}

function schedule(over: Partial<ScheduleRow> = {}): ScheduleRow {
  return {
    id: 'sch-1',
    playbook: 'survey',
    enabled: true,
    next_due_at: '2026-09-02T06:00:00Z',
    consecutive_failures: 0,
    owner_signin_required: false,
    owner_refresh_error: null,
    owner_refresh_at: null,
    ...over,
  };
}

describe('attentionItems', () => {
  it('is empty when nothing waits on anyone', () => {
    expect(attentionItems([run(), run({ status: 'running' })], [schedule()], 0)).toEqual([]);
  });

  it('lists a parked run with its reason and links to its launch', () => {
    const items = attentionItems(
      [run({ key: 'playbook:triage:9', playbook: 'triage', status: 'parked', parked_reason: 'secrets: no pr_token' })],
      [],
      0,
    );
    expect(items).toEqual([
      {
        key: 'parked:playbook:triage:9',
        tone: 'amber',
        text: 'triage is parked: secrets: no pr_token',
        to: '/playbook-runs/playbook%3Atriage%3A9',
      },
    ]);
  });

  it('says so when a parked run has no reason', () => {
    const [item] = attentionItems([run({ status: 'parked' })], [], 0);
    expect(item?.text).toBe('survey is parked: no reason recorded');
  });

  it('collapses parked runs past the cap into one count', () => {
    const parked = Array.from({ length: PARKED_SHOWN + 2 }, (_, i) =>
      run({ key: `playbook:survey:${i}`, status: 'parked' }),
    );
    const items = attentionItems(parked, [], 0);
    expect(items).toHaveLength(PARKED_SHOWN + 1);
    expect(items.at(-1)).toEqual({ key: 'parked:more', tone: 'amber', text: '2 more parked runs', to: '/playbook-runs' });
  });

  it('uses the singular for one extra parked run', () => {
    const parked = Array.from({ length: PARKED_SHOWN + 1 }, (_, i) =>
      run({ key: `playbook:survey:${i}`, status: 'parked' }),
    );
    expect(attentionItems(parked, [], 0).at(-1)?.text).toBe('1 more parked run');
  });

  it('puts schedules waiting on sign-in first, in red', () => {
    const items = attentionItems(
      [run({ status: 'parked' })],
      [schedule({ owner_signin_required: true }), schedule({ id: 'sch-2', owner_signin_required: true })],
      0,
    );
    expect(items[0]).toEqual({
      key: 'schedules:signin',
      tone: 'red',
      text: '2 schedules stop firing until their owners sign in again',
      to: '/schedules',
    });
    expect(items[1]?.key).toBe('parked:playbook:survey:0001');
  });

  it('flags a schedule disabled by failures but not one disabled by hand', () => {
    const items = attentionItems(
      [],
      [schedule({ enabled: false, consecutive_failures: 3 }), schedule({ id: 'sch-2', enabled: false })],
      0,
    );
    expect(items).toEqual([
      {
        key: 'schedules:stopped',
        tone: 'amber',
        text: '1 schedule was disabled after failing to launch',
        to: '/schedules',
      },
    ]);
  });

  it('does not double count a schedule that needs sign-in and is disabled', () => {
    const items = attentionItems(
      [],
      [schedule({ enabled: false, consecutive_failures: 2, owner_signin_required: true })],
      0,
    );
    expect(items.map((i) => i.text)).toEqual(['1 schedule stops firing until its owner signs in again']);
  });

  it('counts the approvals queue', () => {
    expect(attentionItems([], [], 1)).toEqual([
      { key: 'approvals', tone: 'amber', text: '1 item is waiting for approval', to: '/approvals' },
    ]);
    expect(attentionItems([], [], 4)[0]?.text).toBe('4 items are waiting for approval');
  });
});

describe('playbooksByUse', () => {
  const books = [
    { id: 'old', updated_at: '2026-01-01T00:00:00Z' },
    { id: 'fresh', updated_at: '2026-09-01T00:00:00Z' },
    { id: 'launched', updated_at: '2025-01-01T00:00:00Z' },
    { id: 'launched-earlier', updated_at: '2025-01-01T00:00:00Z' },
  ];

  it('puts the latest launched first, then the rest by update', () => {
    const runs = [
      run({ playbook: 'launched-earlier', created_at: '2026-09-10T00:00:00Z' }),
      run({ playbook: 'launched', created_at: '2026-09-20T00:00:00Z' }),
      run({ playbook: 'launched-earlier', created_at: '2026-09-05T00:00:00Z' }),
    ];
    expect(playbooksByUse(books, runs, 10).map((b) => b.id)).toEqual([
      'launched',
      'launched-earlier',
      'fresh',
      'old',
    ]);
  });

  it('ignores runs of playbooks no longer registered', () => {
    const runs = [run({ playbook: 'gone', created_at: '2026-09-30T00:00:00Z' })];
    expect(playbooksByUse(books, runs, 10).map((b) => b.id)).toEqual([
      'fresh',
      'old',
      'launched',
      'launched-earlier',
    ]);
  });

  it('caps at the limit without reordering the input', () => {
    const input = [...books];
    expect(playbooksByUse(input, [], 2).map((b) => b.id)).toEqual(['fresh', 'old']);
    expect(input).toEqual(books);
  });
});

describe('upcoming', () => {
  it('returns firing schedules soonest first, capped', () => {
    const rows = [
      schedule({ id: 'late', next_due_at: '2026-09-03T00:00:00Z' }),
      schedule({ id: 'soon', next_due_at: '2026-09-02T00:00:00Z' }),
      schedule({ id: 'latest', next_due_at: '2026-09-04T00:00:00Z' }),
    ];
    expect(upcoming(rows, 2).map((r) => r.id)).toEqual(['soon', 'late']);
  });

  it('skips schedules that will not fire', () => {
    const rows = [
      schedule({ id: 'off', enabled: false }),
      schedule({ id: 'signin', owner_signin_required: true }),
      schedule({ id: 'done', next_due_at: null }),
      schedule({ id: 'live' }),
    ];
    expect(upcoming(rows, 10).map((r) => r.id)).toEqual(['live']);
  });
});

describe('untilTime', () => {
  const now = Date.parse('2026-09-01T00:00:00Z');

  it('reads minutes, hours and days ahead', () => {
    expect(untilTime('2026-09-01T00:05:00Z', now)).toBe('in 5m');
    expect(untilTime('2026-09-01T03:30:00Z', now)).toBe('in 3h');
    expect(untilTime('2026-09-03T01:00:00Z', now)).toBe('in 2d');
  });

  it('says now for a stamp under a minute out or already passed', () => {
    expect(untilTime('2026-09-01T00:00:30Z', now)).toBe('now');
    expect(untilTime('2026-08-31T00:00:00Z', now)).toBe('now');
  });

  it('is null for a missing or unparseable stamp', () => {
    expect(untilTime(null, now)).toBeNull();
    expect(untilTime(undefined, now)).toBeNull();
    expect(untilTime('not a date', now)).toBeNull();
  });
});

describe('budgetAttention', () => {
  const noon = new Date('2026-08-24T12:00:00Z');

  it('raises a red item once the shared budget is spent', () => {
    expect(budgetAttention({ spent: 1500, ceiling: 1500 }, noon)).toEqual({
      key: 'budget',
      tone: 'red',
      text: 'Shared budget spent. Launches resume in 12h.',
      to: '/playbook-runs',
    });
  });

  it('stays quiet below the ceiling, near it, or uncapped', () => {
    expect(budgetAttention({ spent: 10, ceiling: 1500 }, noon)).toBeNull();
    expect(budgetAttention({ spent: 1400, ceiling: 1500 }, noon)).toBeNull();
    expect(budgetAttention({ spent: 99999, ceiling: null }, noon)).toBeNull();
  });
});
