import { describe, expect, it } from 'vitest';
import { asBodies, keepsAnOwner, withMember, withoutMember, withRole, type MemberBody } from './membersView';

const BASE: MemberBody[] = [
  { kind: 'user', member: 'alice', role: 'owner' },
  { kind: 'group', member: '/groups/x', role: 'member' },
];

describe('membersView', () => {
  it('adds, replaces, removes, and re-roles rows by kind and member', () => {
    const added = withMember(BASE, { kind: 'user', member: ' bob ', role: 'member' });
    expect(added.at(-1)).toEqual({ kind: 'user', member: 'bob', role: 'member' });
    const replaced = withMember(added, { kind: 'user', member: 'bob', role: 'maintainer' });
    expect(replaced.filter((m) => m.member === 'bob')).toEqual([{ kind: 'user', member: 'bob', role: 'maintainer' }]);
    expect(withoutMember(replaced, 'group', '/groups/x').map((m) => m.member)).toEqual(['alice', 'bob']);
    expect(withRole(replaced, 'user', 'alice', 'member')[0]?.role).toBe('member');
  });

  it('refuses to drop the last user at owner', () => {
    expect(keepsAnOwner(BASE)).toBe(true);
    expect(keepsAnOwner(withRole(BASE, 'user', 'alice', 'maintainer'))).toBe(false);
    expect(keepsAnOwner([{ kind: 'team', member: 'core', role: 'owner' }])).toBe(false);
  });

  it('sends only kind, member, and role', () => {
    expect(
      asBodies([{ kind: 'user', member: 'alice', role: 'owner', since: 'x', added_by: 'y' }]),
    ).toEqual([{ kind: 'user', member: 'alice', role: 'owner' }]);
  });
});
