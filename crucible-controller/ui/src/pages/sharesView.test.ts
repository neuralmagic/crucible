import { describe, expect, it } from 'vitest';
import { expiryLabel, notAfter, sortedShares, validGrantee, type ShareDto } from './sharesView';

function share(grantee: string, expired: boolean, not_after: string | null): ShareDto {
  return {
    grantee,
    role: 'viewer',
    not_after,
    expired,
    created_by: 'user:alice',
    created_at: '2026-09-14T00:00:00Z',
    updated_at: '2026-09-14T00:00:00Z',
  };
}

describe('sharesView', () => {
  it('accepts a user or team grantee and nothing else', () => {
    expect(validGrantee('user:bob')).toBe(true);
    expect(validGrantee(' team:llm-d ')).toBe(true);
    expect(validGrantee('group:/x')).toBe(false);
    expect(validGrantee('bob')).toBe(false);
    expect(validGrantee('user:')).toBe(false);
  });

  it('turns a day into the end of that day, and blank into no expiry', () => {
    expect(notAfter('2026-10-01')).toBe('2026-10-01T23:59:59Z');
    expect(notAfter('  ')).toBeNull();
  });

  it('lists live shares before expired ones and names the expiry', () => {
    const rows = sortedShares([
      share('user:zed', true, '2020-01-01T00:00:00Z'),
      share('user:bob', false, null),
      share('team:llm-d', false, '2027-01-01T00:00:00Z'),
    ]);
    expect(rows.map((r) => r.grantee)).toEqual(['team:llm-d', 'user:bob', 'user:zed']);
    expect(rows.map(expiryLabel)).toEqual(['2027-01-01T00:00:00Z', 'never', 'expired 2020-01-01T00:00:00Z']);
  });
});
