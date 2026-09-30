import { describe, expect, it } from 'vitest';
import { byField, celCompletions, markersOf, type CelLanguageDto } from './celTooling';

const LANGUAGE: CelLanguageDto = {
  variables: ['body', 'headers', 'delivery', 'received_at'],
  functions: [
    { name: 'size', member: false },
    { name: 'string', member: false },
    { name: 'startsWith', member: true },
    { name: 'matches', member: true },
  ],
  macros: ['has', 'exists', 'map'],
};

function labels(prefix: string): string[] {
  return celCompletions(LANGUAGE).complete(prefix)?.items.map((i) => i.label) ?? [];
}

describe('celCompletions', () => {
  it('offers variables, global functions, and has at the start of a term', () => {
    expect(labels('')).toEqual(['body', 'headers', 'delivery', 'received_at', 'size', 'string', 'has']);
    expect(labels('size(b')).toEqual(['body']);
  });

  it('offers member functions and macros after a dot', () => {
    expect(labels('body.repository.')).toEqual(['startsWith', 'matches', 'exists', 'map']);
    expect(labels('body.tags.ex')).toEqual(['exists']);
  });

  it('replaces from the start of the word being typed', () => {
    expect(celCompletions(LANGUAGE).complete('body.tags.ex')?.from).toBe(10);
    expect(celCompletions(LANGUAGE).complete('body.')?.from).toBe(5);
  });
});

describe('markersOf', () => {
  it('places a positioned diagnostic and spans the expression otherwise', () => {
    const source = 'body.a ==\n  1';
    expect(
      markersOf(
        [
          { field: 'filter', message: 'Syntax error', line: 1, column: 10 },
          { field: 'filter', message: 'calls join', line: null, column: null },
        ],
        source
      )
    ).toEqual([
      { line: 1, col: 10, message: 'Syntax error' },
      { line: 1, col: 1, endLine: 2, endCol: 4, message: 'calls join' },
    ]);
  });
});

describe('byField', () => {
  it('groups by field', () => {
    const grouped = byField([
      { field: 'filter', message: 'a', line: null, column: null },
      { field: 'derive.image', message: 'b', line: null, column: null },
      { field: 'filter', message: 'c', line: null, column: null },
    ]);
    expect(grouped.get('filter')?.map((d) => d.message)).toEqual(['a', 'c']);
    expect(grouped.get('derive.image')?.map((d) => d.message)).toEqual(['b']);
  });
});
