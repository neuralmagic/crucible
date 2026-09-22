import { describe, expect, it } from 'vitest';
import { ISSUE_STATUSES, ISSUE_STATUS_COLORS } from './issueStatus';

describe('ISSUE_STATUSES', () => {
  it('pins the exact kebab-case vocabulary Status::parse accepts (model.rs)', () => {
    expect([...ISSUE_STATUSES]).toEqual([
      'new',
      'scoped',
      'awaiting-approval',
      'building',
      'running',
      'pr-open',
      'parked',
      'done',
    ]);
  });

  it('never regresses to underscored spellings (they 400 the API)', () => {
    for (const status of ISSUE_STATUSES) {
      expect(status).not.toContain('_');
    }
  });

  it('colors every status (the Record type is the exhaustiveness guard)', () => {
    expect(Object.keys(ISSUE_STATUS_COLORS).sort()).toEqual([...ISSUE_STATUSES].sort());
  });
});
