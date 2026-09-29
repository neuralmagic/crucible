import { describe, expect, it } from 'vitest';
import { HELD, sortedMembers, sortedTeams, validSlug, viewAsOffer, type MemberDto, type TeamDto } from './teamsView';

function member(kind: MemberDto['kind'], name: string, role: MemberDto['role'], signed_in = false): MemberDto {
  return { kind, member: name, role, since: '2026-09-13T00:00:00Z', added_by: null, signed_in };
}

function team(slug: string, my_role: TeamDto['my_role']): TeamDto {
  return {
    slug,
    display_name: slug,
    members: [],
    my_role,
    reachable: true,
    created_at: '2026-09-13T00:00:00Z',
    created_by: null,
    updated_at: '2026-09-13T00:00:00Z',
  };
}

describe('teamsView', () => {
  it('names how each kind of member row is held', () => {
    expect(HELD).toEqual({ user: 'direct', group: 'group', team: 'nested team', rule: 'rule' });
  });

  it('orders members owners first, then by how they are held, then by name', () => {
    const rows = sortedMembers([
      member('rule', 'email-domain:example.com', 'member'),
      member('user', 'zed', 'member'),
      member('group', '/groups/x', 'maintainer'),
      member('user', 'alice', 'owner'),
      member('team', 'core', 'member'),
    ]);
    expect(rows.map((r) => r.member)).toEqual(['alice', '/groups/x', 'zed', 'core', 'email-domain:example.com']);
  });

  it('offers view-as only on user rows that have signed in, and never on the viewer', () => {
    expect(viewAsOffer(member('user', 'reed', 'member', true), 'root')).toBe('ready');
    expect(viewAsOffer(member('user', 'ghost', 'member'), 'root')).toBe('never-signed-in');
    expect(viewAsOffer(member('user', 'root', 'owner', true), 'root')).toBe('none');
    expect(viewAsOffer(member('user', 'reed', 'member', true), null)).toBe('none');
    for (const kind of ['group', 'team', 'rule'] as const) {
      expect(viewAsOffer(member(kind, 'x', 'member', true), 'root')).toBe('none');
    }
  });

  it('lists the teams the caller is in before the rest', () => {
    const rows = sortedTeams([team('zeta', null), team('llm-d', 'member'), team('alpha', undefined), team('core', 'owner')]);
    expect(rows.map((r) => r.slug)).toEqual(['core', 'llm-d', 'alpha', 'zeta']);
  });

  it('accepts a lowercase dashed slug and nothing else', () => {
    expect(validSlug('llm-d')).toBe(true);
    expect(validSlug('LLM')).toBe(false);
    expect(validSlug('1st')).toBe(false);
    expect(validSlug('')).toBe(false);
  });
});
