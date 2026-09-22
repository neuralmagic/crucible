import { describe, expect, it } from 'vitest';
import { parsePrNumber } from './prNumber';

describe('parsePrNumber', () => {
  it('parses a plain pull-request url', () => {
    expect(parsePrNumber('https://github.com/owner/repo/pull/123')).toBe(123);
  });

  it('tolerates trailing segments, query strings, and anchors', () => {
    expect(parsePrNumber('https://github.com/o/r/pull/7/files')).toBe(7);
    expect(parsePrNumber('https://github.com/o/r/pull/7?diff=split')).toBe(7);
    expect(parsePrNumber('https://github.com/o/r/pull/7#discussion_r1')).toBe(7);
  });

  it('returns null for urls without a pull number', () => {
    expect(parsePrNumber('https://github.com/o/r/issues/7')).toBeNull();
    expect(parsePrNumber('https://github.com/o/r/pull/')).toBeNull();
    expect(parsePrNumber('not a url')).toBeNull();
    expect(parsePrNumber('https://github.com/o/r/pull/notanumber')).toBeNull();
  });
});
