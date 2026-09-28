import { describe, expect, it } from 'vitest';
import { budgetNotice, budgetPercent, budgetState, resetsIn, usd, yourSpendToday } from './budget';

describe('budgetState', () => {
  it('is uncapped without a ceiling', () => {
    expect(budgetState({ spent: 900, ceiling: null })).toBe('uncapped');
    expect(budgetState({ spent: 900, ceiling: undefined })).toBe('uncapped');
  });

  it('reads ok, near, then spent as the ceiling fills', () => {
    expect(budgetState({ spent: 0, ceiling: 100 })).toBe('ok');
    expect(budgetState({ spent: 79.99, ceiling: 100 })).toBe('ok');
    expect(budgetState({ spent: 80, ceiling: 100 })).toBe('near');
    expect(budgetState({ spent: 100, ceiling: 100 })).toBe('spent');
    expect(budgetState({ spent: 140, ceiling: 100 })).toBe('spent');
  });

  it('treats a zero ceiling as spent', () => {
    expect(budgetState({ spent: 0, ceiling: 0 })).toBe('spent');
  });
});

describe('budgetPercent', () => {
  it('rounds to a whole percent', () => {
    expect(budgetPercent({ spent: 412.8, ceiling: 1500 })).toBe(28);
  });

  it('is null uncapped and full at a zero ceiling', () => {
    expect(budgetPercent({ spent: 5, ceiling: null })).toBeNull();
    expect(budgetPercent({ spent: 0, ceiling: 0 })).toBe(100);
  });
});

describe('usd', () => {
  it('always shows cents and groups thousands', () => {
    expect(usd(0)).toBe('$0.00');
    expect(usd(1500)).toBe('$1,500.00');
    expect(usd(8.4)).toBe('$8.40');
  });
});

describe('resetsIn', () => {
  it('counts down to the next UTC midnight', () => {
    expect(resetsIn(new Date('2026-08-24T12:00:00Z'))).toBe('12h');
    expect(resetsIn(new Date('2026-08-24T22:30:00Z'))).toBe('1h 30m');
    expect(resetsIn(new Date('2026-08-24T23:59:30Z'))).toBe('1m');
    expect(resetsIn(new Date('2026-08-24T00:00:00Z'))).toBe('24h');
  });

  it('rolls over month ends', () => {
    expect(resetsIn(new Date('2026-08-31T20:00:00Z'))).toBe('4h');
  });
});

describe('yourSpendToday', () => {
  const now = new Date('2026-08-24T12:00:00Z');
  const runs = [
    { created_by: 'wren', created_at: '2026-08-24T09:00:00Z', cost_usd: 1.25 },
    { created_by: 'Wren', created_at: '2026-08-24T00:00:00Z', cost_usd: 2 },
    { created_by: 'wren', created_at: '2026-08-24T10:00:00Z', cost_usd: null },
    { created_by: 'wren', created_at: '2026-08-23T23:59:59Z', cost_usd: 50 },
    { created_by: 'kylesayrs', created_at: '2026-08-24T09:00:00Z', cost_usd: 7 },
    { created_by: null, created_at: '2026-08-24T09:00:00Z', cost_usd: 9 },
  ];

  it('sums the caller runs launched today, case-insensitively', () => {
    expect(yourSpendToday(runs, 'wren', now)).toBeCloseTo(3.25);
    expect(yourSpendToday(runs, ' WREN ', now)).toBeCloseTo(3.25);
  });

  it('is zero for someone with no runs today', () => {
    expect(yourSpendToday(runs, 'nobody', now)).toBe(0);
  });
});

describe('budgetNotice', () => {
  const noon = new Date('2026-08-24T12:00:00Z');

  it('says how much of the shared budget is used', () => {
    expect(budgetNotice({ spent: 412.8, ceiling: 1500 }, noon)).toBe('$412.80 of $1,500.00 used today.');
  });

  it('says when launches resume once it is spent', () => {
    expect(budgetNotice({ spent: 1600, ceiling: 1500 }, noon)).toBe('Spent. Launches resume in 12h.');
  });

  it('is null without a ceiling', () => {
    expect(budgetNotice({ spent: 5, ceiling: null }, noon)).toBeNull();
  });
});
