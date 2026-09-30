import { describe, expect, it } from 'vitest';
import { runLabel } from './runLabel';

describe('runLabel', () => {
  it('reads a run by its name', () => {
    expect(runLabel({ run_id: 'playbook_7-1730000000', name: 'benevolent-monkey' })).toBe(
      'benevolent-monkey',
    );
  });

  it('falls back to the id for a run from before names', () => {
    expect(runLabel({ run_id: 'owner_repo_7-1720000000', name: null })).toBe('owner_repo_7-1720000000');
    expect(runLabel({ run_id: 'owner_repo_7-1720000000' })).toBe('owner_repo_7-1720000000');
  });
});
