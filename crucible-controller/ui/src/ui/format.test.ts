import { describe, expect, it } from 'vitest';
import { formatStamp, middleTruncate } from './format';

describe('formatStamp', () => {
  it('localizes valid timestamps and falls back on the raw string', () => {
    const rfc = '2026-08-12T03:15:00Z';
    expect(formatStamp(rfc)).toBe(new Date(rfc).toLocaleString());
    expect(formatStamp('not-a-date')).toBe('not-a-date');
    expect(formatStamp(null)).toBe('—');
    expect(formatStamp(undefined)).toBe('—');
  });
});

describe('middleTruncate', () => {
  it('cuts a long value in the middle and leaves a short one whole', () => {
    expect(middleTruncate('sha256:8484c6b8284cf62cf60c16bcd090795c', 10, 6)).toBe('sha256:848\u202690795c');
    expect(middleTruncate('abcdefghij', 4, 4)).toBe('abcd\u2026ghij');
    expect(middleTruncate('abcdefghi', 4, 4)).toBe('abcdefghi');
    expect(middleTruncate('', 4, 4)).toBe('');
  });
});
