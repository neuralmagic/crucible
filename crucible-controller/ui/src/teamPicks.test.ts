import { describe, expect, it } from 'vitest';
import { activeTeams, matchesTeam, pickTeams } from './teamPicks';

describe('active teams', () => {
  it('collects the teams among owners and skips users, groups and blanks', () => {
    expect([...activeTeams(['team:llm-d', 'user:wren', 'group:/x', null, undefined, 'team:llm-d', 'team:infra'])].sort()).toEqual([
      'infra',
      'llm-d',
    ]);
  });
});

describe('picking teams to name', () => {
  const teams = ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'];

  it('names starred teams first, then active ones up to the limit', () => {
    const { shown, hidden } = pickTeams(teams, ['h'], new Set(['a', 'b', 'c', 'h']), null, 2);
    expect(shown).toEqual([
      { slug: 'h', starred: true },
      { slug: 'a', starred: false },
      { slug: 'b', starred: false },
    ]);
    expect(hidden).toBe(5);
  });

  it('never caps starred teams, and ignores stars on teams the caller left', () => {
    const { shown } = pickTeams(['a', 'b', 'c', 'd'], ['a', 'b', 'c', 'gone'], new Set(), null, 1);
    expect(shown.map((t) => t.slug)).toEqual(['a', 'b', 'c']);
  });

  it('names the current team even when it is neither starred nor active', () => {
    const { shown, hidden } = pickTeams(teams, [], new Set(['a']), 'g', 6);
    expect(shown.map((t) => t.slug)).toEqual(['a', 'g']);
    expect(hidden).toBe(6);
  });

  it('names every team, starred first, for a caller in only a few', () => {
    const { shown, hidden } = pickTeams(['b', 'a', 'c'], ['c'], new Set(), null, 6);
    expect(shown).toEqual([
      { slug: 'c', starred: true },
      { slug: 'a', starred: false },
      { slug: 'b', starred: false },
    ]);
    expect(hidden).toBe(0);
  });

  it('names nothing for a caller in no team', () => {
    expect(pickTeams([], ['a'], new Set(['a']), 'a')).toEqual({ shown: [], hidden: 0 });
  });
});

describe('filtering teams', () => {
  it('matches slug or name, case-insensitively, and an empty query matches all', () => {
    expect(matchesTeam('', 'llm-d', 'LLM-D')).toBe(true);
    expect(matchesTeam('  LLM ', 'llm-d', 'x')).toBe(true);
    expect(matchesTeam('infer', 'llm-d', 'Inference Engineering')).toBe(true);
    expect(matchesTeam('gpu', 'llm-d', 'LLM-D')).toBe(false);
  });
});
