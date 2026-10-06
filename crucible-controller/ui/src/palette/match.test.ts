import { describe, expect, it } from 'vitest';

import { score } from './match';

describe('score', () => {
  it('shows everything for an empty search', () => {
    expect(score('', 'anything', [])).toBe(1);
    expect(score('  -- ', 'anything', [])).toBe(1);
  });

  it('ranks title prefix over word prefix over substring over other fields', () => {
    const ranks = [
      score('edit', 'Edit notes', []),
      score('edit', 'Quick edit', []),
      score('edit', 'Unedited', []),
      score('edit', 'Secrets', ['edit history']),
      score('edit', 'Secrets', ['credited']),
    ];
    expect(ranks).toEqual([1, 0.8, 0.6, 0.4, 0.2]);
  });

  it('never matches scattered letters', () => {
    expect(score('edit', 'Secrets', ['credentials', 'api key'])).toBe(0);
  });

  it('needs every term, averaging their strength', () => {
    expect(score('edit notes', 'Edit notes', [])).toBe(0.9);
    expect(score('edit missing', 'Edit notes', [])).toBe(0);
    expect(score('cve run', 'cve-triage', ['running'])).toBeCloseTo(0.7);
  });

  it('ignores case and punctuation', () => {
    expect(score('PLAYBOOK:CVE', 'playbook:cve-triage:01a1', [])).toBe(0.9);
  });
});
