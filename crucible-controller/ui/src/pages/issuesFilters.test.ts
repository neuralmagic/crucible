import { describe, expect, it } from 'vitest';
import {
  DEFAULT_RECENCY,
  DEFAULT_SORT,
  ISSUE_KINDS,
  ISSUE_KIND_LABELS,
  parseKind,
  parseRecency,
  parseSortDir,
  parseSortKey,
  parseUpstream,
  recencyCutoff,
  splitRepo,
} from './issuesFilters';

describe('the input-kind vocabulary', () => {
  // `?kind=` is matched server-side against the stored `input_kind` tag, which is the same string
  // the DTO serializes as its `type` discriminant. A value that isn't one of those tags round-trips
  // fine through the URL and the Select and then matches zero rows, which reads as an empty backlog
  // rather than a bug — so pin the spellings, scenario included.
  it('offers every labeled kind, spelled as its wire tag', () => {
    expect([...ISSUE_KINDS].sort()).toEqual(Object.keys(ISSUE_KIND_LABELS).sort());
    expect([...ISSUE_KINDS].sort()).toEqual(['github', 'jira', 'playbook', 'scenario']);
  });

  it('parses each kind back to itself', () => {
    for (const kind of ISSUE_KINDS) {
      expect(parseKind(kind)).toBe(kind);
    }
    expect(parseKind('scenario')).toBe('scenario');
  });

  it('rejects near-misses and display names instead of passing them to the API', () => {
    expect(parseKind('Scenario')).toBe('');
    expect(parseKind('scenarios')).toBe('');
    expect(parseKind('unknown')).toBe('');
    expect(parseKind(null)).toBe('');
    expect(parseKind('')).toBe('');
  });
});

describe('parseRecency', () => {
  it('accepts every preset', () => {
    expect(parseRecency('7d')).toBe('7d');
    expect(parseRecency('30d')).toBe('30d');
    expect(parseRecency('90d')).toBe('90d');
    expect(parseRecency('1y')).toBe('1y');
    expect(parseRecency('all')).toBe('all');
  });

  it('falls back to the default on garbage or absence', () => {
    expect(parseRecency(null)).toBe(DEFAULT_RECENCY);
    expect(parseRecency('')).toBe(DEFAULT_RECENCY);
    expect(parseRecency('2weeks')).toBe(DEFAULT_RECENCY);
  });
});

describe('recencyCutoff', () => {
  const now = new Date('2026-07-05T12:00:00.500Z');

  it('subtracts the preset window at second resolution', () => {
    expect(recencyCutoff('7d', now)).toBe('2026-06-28T12:00:00Z');
    expect(recencyCutoff('30d', now)).toBe('2026-06-05T12:00:00Z');
    expect(recencyCutoff('90d', now)).toBe('2026-04-06T12:00:00Z');
    expect(recencyCutoff('1y', now)).toBe('2025-07-05T12:00:00Z');
  });

  it('emits no cutoff for all time', () => {
    expect(recencyCutoff('all', now)).toBeUndefined();
  });

  it('never carries milliseconds (lexical comparison against DB stamps)', () => {
    const cutoff = recencyCutoff('7d', new Date('2026-01-15T08:30:45.999Z'));
    expect(cutoff).toBe('2026-01-08T08:30:45Z');
  });
});

describe('parseUpstream', () => {
  it('accepts the wire vocabulary', () => {
    expect(parseUpstream('open')).toBe('open');
    expect(parseUpstream('closed')).toBe('closed');
  });

  it('treats anything else as no filter', () => {
    expect(parseUpstream(null)).toBe('');
    expect(parseUpstream('merged')).toBe('');
  });
});

describe('parseSortKey / parseSortDir', () => {
  it('accepts the wire sort vocabulary', () => {
    expect(parseSortKey('updated')).toBe('updated');
    expect(parseSortKey('upstream')).toBe('upstream');
    expect(parseSortKey('tier')).toBe('tier');
    expect(parseSortKey('priority')).toBe('priority');
    expect(parseSortKey('title')).toBe('title');
  });

  it('defaults to upstream recency', () => {
    expect(parseSortKey(null)).toBe(DEFAULT_SORT);
    expect(parseSortKey('bogus')).toBe('upstream');
  });

  it('only asc opts out of the descending default', () => {
    expect(parseSortDir('asc')).toBe('asc');
    expect(parseSortDir('desc')).toBe('desc');
    expect(parseSortDir(null)).toBe('desc');
    expect(parseSortDir('up')).toBe('desc');
  });
});

describe('splitRepo', () => {
  it('splits owner from name at the first slash', () => {
    expect(splitRepo('llm-d/llm-d-router')).toEqual({ owner: 'llm-d', name: 'llm-d-router' });
  });

  it('keeps nested paths in the name', () => {
    expect(splitRepo('org/group/repo')).toEqual({ owner: 'org', name: 'group/repo' });
  });

  it('handles a slash-less value', () => {
    expect(splitRepo('standalone')).toEqual({ owner: null, name: 'standalone' });
  });
});
