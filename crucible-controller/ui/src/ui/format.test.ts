import { describe, expect, it } from 'vitest';
import { formatStamp } from './format';

describe('formatStamp', () => {
  it('localizes valid timestamps and falls back on the raw string', () => {
    const rfc = '2026-08-12T03:15:00Z';
    expect(formatStamp(rfc)).toBe(new Date(rfc).toLocaleString());
    expect(formatStamp('not-a-date')).toBe('not-a-date');
    expect(formatStamp(null)).toBe('—');
    expect(formatStamp(undefined)).toBe('—');
  });
});
