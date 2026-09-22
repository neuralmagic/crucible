import { describe, expect, it } from 'vitest';
import { formatCell } from './db';

describe('formatCell', () => {
  it('renders every arrow value shape compactly', () => {
    expect(formatCell(null)).toBe('∅');
    expect(formatCell(undefined)).toBe('∅');
    expect(formatCell(42)).toBe('42');
    expect(formatCell(9007199254740993n)).toBe('9007199254740993');
    expect(formatCell(true)).toBe('true');
    expect(formatCell('text')).toBe('text');
    expect(formatCell(new Date('2026-07-04T00:00:00Z'))).toBe('2026-07-04T00:00:00.000Z');
    expect(formatCell(new Uint8Array(5))).toBe('bytes[5]');
    expect(formatCell({ nested: 1 })).toBe('{"nested":1}');
  });
});
