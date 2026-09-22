import { describe, expect, it } from 'vitest';
import type { components } from './api/schema';
import {
  ALL,
  actedAs,
  defaultOwner,
  effectiveContext,
  narrow,
  ownerOptions,
  switchable,
  viaLabel,
  withOwner,
} from './ownerContext';

type Whoami = components['schemas']['Whoami'];

function whoami(overrides: Partial<Whoami> = {}): Whoami {
  return {
    user: 'Alice',
    admin: false,
    role: 'viewer',
    groups: ['/groups/x', '/groups/x', ' '],
    mode: 'native',
    downgraded: false,
    proves_groups: true,
    teams: [
      { team: 'zeta', role: 'member', via: [{ kind: 'direct', role: 'member' }] },
      { team: 'llm-d', role: 'maintainer', via: [{ kind: 'group', group: '/groups/x', role: 'maintainer' }] },
    ],
    ...overrides,
  };
}

describe('actedAs', () => {
  it('lists the user, teams by slug at their role, then proven groups once', () => {
    expect(actedAs(whoami()).map((p) => [p.value, p.kind, p.role])).toEqual([
      ['user:alice', 'user', 'owner'],
      ['team:llm-d', 'team', 'maintainer'],
      ['team:zeta', 'team', 'member'],
      ['group:/groups/x', 'group', 'owner'],
    ]);
  });

  it('drops groups a credential does not prove and names nobody when anonymous', () => {
    expect(actedAs(whoami({ proves_groups: false })).map((p) => p.value)).toEqual([
      'user:alice',
      'team:llm-d',
      'team:zeta',
    ]);
    expect(actedAs(whoami({ user: null, teams: [], groups: [] }))).toEqual([]);
    expect(actedAs(undefined)).toEqual([]);
  });
});

describe('ownerOptions and switchable', () => {
  it('offers the user, maintained teams, and groups as owners; the user and every team to switch to', () => {
    const principals = actedAs(whoami());
    expect(ownerOptions(principals).map((p) => p.value)).toEqual([
      'user:alice',
      'team:llm-d',
      'group:/groups/x',
    ]);
    expect(switchable(principals).map((p) => p.value)).toEqual(['user:alice', 'team:llm-d', 'team:zeta']);
  });
});

describe('context', () => {
  const principals = actedAs(whoami());

  it('keeps a stored context the caller still acts as and falls back to all otherwise', () => {
    expect(effectiveContext(null, principals)).toBe(ALL);
    expect(effectiveContext('team:llm-d', principals)).toBe('team:llm-d');
    expect(effectiveContext('team:gone', principals)).toBe(ALL);
  });

  it('narrows rows to the owner in context and leaves all alone', () => {
    const rows = [
      { id: 'a', owner: 'user:alice' },
      { id: 'b', owner: 'team:llm-d' },
      { id: 'c', owner: null },
    ];
    expect(narrow(rows, ALL, (r) => r.owner).map((r) => r.id)).toEqual(['a', 'b', 'c']);
    expect(narrow(rows, 'team:llm-d', (r) => r.owner).map((r) => r.id)).toEqual(['b']);
    expect(narrow(rows, 'team:zeta', (r) => r.owner)).toEqual([]);
  });

  it('starts a form on the context when it may own, else the first owner, and keeps a held choice', () => {
    const options = ownerOptions(principals);
    expect(defaultOwner('team:llm-d', options)).toBe('team:llm-d');
    expect(defaultOwner('team:zeta', options)).toBe('user:alice');
    expect(defaultOwner(ALL, options)).toBe('user:alice');
    expect(defaultOwner(ALL, [])).toBe('');
    expect(withOwner('group:/groups/x', ALL, options)).toBe('group:/groups/x');
    expect(withOwner('team:zeta', 'team:llm-d', options)).toBe('team:llm-d');
  });
});

describe('viaLabel', () => {
  it('names each way a membership is held', () => {
    expect(viaLabel({ kind: 'direct', role: 'owner' })).toBe('direct · owner');
    expect(viaLabel({ kind: 'group', group: '/groups/x', role: 'member' })).toBe('group /groups/x · member');
    expect(viaLabel({ kind: 'rule', rule: 'configured-admins', role: 'owner' })).toBe('rule configured-admins · owner');
    expect(viaLabel({ kind: 'team', team: 'parent', role: 'maintainer' })).toBe('team parent · maintainer');
  });
});
