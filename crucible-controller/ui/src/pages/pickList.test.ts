import { describe, expect, it } from 'vitest';
import { arrangeOptions, optionValue, principalOptions, toggleFavorite } from './pickList';

const options = [
  { value: 'user:alice', label: 'user:alice' },
  { value: 'group:/groups/team-x', label: 'group:/groups/team-x' },
  { value: 'group:/groups/team-y', label: 'group:/groups/team-y' },
  { value: 'group:/groups/infra', label: 'group:/groups/infra' },
];

describe('arrangeOptions', () => {
  it('keeps the given order with nothing starred and no filter', () => {
    expect(arrangeOptions(options, '', []).map((o) => o.value)).toEqual(options.map((o) => o.value));
  });

  it('lifts starred entries to the top in their own order', () => {
    const arranged = arrangeOptions(options, '', ['group:/groups/infra', 'group:/groups/team-x']);
    expect(arranged.map((o) => o.value)).toEqual([
      'group:/groups/team-x',
      'group:/groups/infra',
      'user:alice',
      'group:/groups/team-y',
    ]);
    expect(arranged.map((o) => o.favorite)).toEqual([true, true, false, false]);
  });

  it('filters by a case-insensitive substring of the label or value', () => {
    expect(arrangeOptions(options, 'TEAM', []).map((o) => o.value)).toEqual([
      'group:/groups/team-x',
      'group:/groups/team-y',
    ]);
    expect(arrangeOptions(options, '  alice ', []).map((o) => o.value)).toEqual(['user:alice']);
    expect(arrangeOptions(options, 'nobody', [])).toEqual([]);
  });

  it('filters the starred entries too', () => {
    const arranged = arrangeOptions(options, 'infra', ['group:/groups/team-x', 'group:/groups/infra']);
    expect(arranged.map((o) => o.value)).toEqual(['group:/groups/infra']);
  });

  it('ignores a favorite the options no longer carry', () => {
    expect(arrangeOptions(options, '', ['group:/gone']).every((o) => !o.favorite)).toBe(true);
  });
});

describe('toggleFavorite', () => {
  it('adds a value that is not starred and removes one that is', () => {
    expect(toggleFavorite([], 'a')).toEqual(['a']);
    expect(toggleFavorite(['a', 'b'], 'a')).toEqual(['b']);
    expect(toggleFavorite(['a'], 'b')).toEqual(['a', 'b']);
  });
});

describe('principalOptions', () => {
  it('offers every principal the caller may own as, naming a team at its role', () => {
    expect(
      principalOptions([
        { value: 'user:will', label: 'will', kind: 'user', role: 'owner' },
        { value: 'team:llm-d', label: 'llm-d', kind: 'team', role: 'maintainer' },
        { value: 'group:/groups/team-x', label: '/groups/team-x', kind: 'group', role: 'owner' },
      ]),
    ).toEqual([
      { value: 'user:will', label: 'user:will' },
      { value: 'team:llm-d', label: 'team:llm-d (maintainer)' },
      { value: 'group:/groups/team-x', label: 'group:/groups/team-x' },
    ]);
  });

  it('offers nothing to a caller with no identity', () => {
    expect(principalOptions([])).toEqual([]);
  });
});

describe('optionValue', () => {
  const kinds: readonly { value: 'opaque' | 'file'; label: string }[] = [
    { value: 'opaque', label: 'opaque' },
    { value: 'file', label: 'file' },
  ];

  it('returns the option the raw string names', () => {
    expect(optionValue(kinds, 'file', 'opaque')).toBe('file');
  });

  it('falls back when the raw string names nothing in the table', () => {
    expect(optionValue(kinds, 'kubeconfig', 'opaque')).toBe('opaque');
    expect(optionValue(kinds, '', 'opaque')).toBe('opaque');
  });
});
