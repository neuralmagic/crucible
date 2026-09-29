import { describe, expect, it } from 'vitest';
import {
  diffRules,
  explainParams,
  newestFirst,
  parsePolicies,
  ruleAnchor,
  ruleDiffIsEmpty,
  shortDigest,
  type PolicySetDto,
} from './policyView';

const SET = `// The header comment; belongs to no rule.

@id("platform-admin-all")
permit(principal, action, resource)
when { principal.platform_admin };

// A comment with a ; inside it.
@id("no-deletes")
forbid(principal, action == Action::"playbook:delete", resource)
unless { principal.hasTag("group:a;b") };
`;

function set(digest: string, created_at: string, active = false): PolicySetDto {
  return {
    digest,
    text: '',
    owner: 'team:platform-administrators',
    schema_version: 1,
    created_at,
    active,
  };
}

describe('parsePolicies', () => {
  it('splits statements at a top-level semicolon and names each by @id', () => {
    const rules = parsePolicies(SET);
    expect(rules.map((r) => [r.id, r.effect, r.line])).toEqual([
      ['platform-admin-all', 'permit', 3],
      ['no-deletes', 'forbid', 8],
    ]);
    expect(rules[0].text).toBe(
      '@id("platform-admin-all")\npermit(principal, action, resource)\nwhen { principal.platform_admin };',
    );
  });

  it('ignores a semicolon inside a string literal or a comment', () => {
    const rules = parsePolicies(SET);
    expect(rules).toHaveLength(2);
    expect(rules[1].text.endsWith('hasTag("group:a;b") };')).toBe(true);
  });

  it('keeps escaped quotes inside an id and a string', () => {
    const [rule] = parsePolicies('@id("say \\"hi\\"")\npermit(principal, action, resource) when { "a\\";" == "b" };');
    expect(rule.id).toBe('say "hi"');
    expect(rule.effect).toBe('permit');
  });

  it('reports a statement without an id or a closing semicolon as it stands', () => {
    const rules = parsePolicies('permit(principal, action, resource);\n@id("half")\nforbid(principal');
    expect(rules.map((r) => [r.id, r.effect, r.line])).toEqual([
      [null, 'permit', 1],
      ['half', 'forbid', 2],
    ]);
    expect(rules[1].text).toBe('@id("half")\nforbid(principal');
  });

  it('finds nothing in an empty or comment-only set', () => {
    expect(parsePolicies('')).toEqual([]);
    expect(parsePolicies('// nothing\n  \n// here\n')).toEqual([]);
  });

  it('reads the effect after several annotations', () => {
    const [rule] = parsePolicies('@id("x")\n@reason("because")\nforbid(principal, action, resource);');
    expect(rule.effect).toBe('forbid');
    expect(rule.id).toBe('x');
  });
});

describe('diffRules', () => {
  it('names rules added, removed, and changed by id', () => {
    const before = parsePolicies(SET);
    const after = parsePolicies(`@id("platform-admin-all")
permit(principal, action, resource)
when { principal.platform_admin && true };

@id("llm-d-devs-autoresearch")
permit(principal, action == Action::"autoresearch:access", resource)
when { principal.hasTag("group:inference-eng-llm-d-devs") };
`);
    const diff = diffRules(before, after);
    expect(diff).toEqual({
      added: ['llm-d-devs-autoresearch'],
      removed: ['no-deletes'],
      changed: ['platform-admin-all'],
    });
    expect(ruleDiffIsEmpty(diff)).toBe(false);
  });

  it('is empty when only comments and spacing between rules moved', () => {
    const moved = `@id("no-deletes")
forbid(principal, action == Action::"playbook:delete", resource)
unless { principal.hasTag("group:a;b") };


@id("platform-admin-all")
permit(principal, action, resource)
when { principal.platform_admin };`;
    expect(ruleDiffIsEmpty(diffRules(parsePolicies(SET), parsePolicies(moved)))).toBe(true);
  });

  it('reads an edited statement without an id as a removal and an addition', () => {
    const diff = diffRules(parsePolicies('permit(principal, action, resource);'), parsePolicies('forbid(principal, action, resource);'));
    expect(diff).toEqual({
      added: ['forbid(principal, action, resource);'],
      removed: ['permit(principal, action, resource);'],
      changed: [],
    });
  });
});

describe('newestFirst', () => {
  it('orders by creation, newest first, without touching the input', () => {
    const sets = [set('a', '2026-09-01T00:00:00Z', true), set('c', '2026-09-03T00:00:00Z'), set('b', '2026-09-02T00:00:00Z')];
    expect(newestFirst(sets).map((s) => s.digest)).toEqual(['c', 'b', 'a']);
    expect(sets[0].digest).toBe('a');
  });
});

describe('shortDigest', () => {
  it('keeps the first twelve hex characters', () => {
    expect(shortDigest('0123456789abcdef0123')).toBe('0123456789ab');
  });
});

describe('ruleAnchor', () => {
  it('is a DOM-safe id per rule', () => {
    expect(ruleAnchor('operators-access-autoresearch')).toBe('rule-operators-access-autoresearch');
    expect(ruleAnchor('a b/"c"')).toBe('rule-a_b__c_');
  });
});

describe('explainParams', () => {
  it('needs a login and an action', () => {
    expect(explainParams('  ', 'autoresearch:access', '')).toBeNull();
    expect(explainParams('reed', '', '')).toBeNull();
  });

  it('trims and leaves out an empty resource', () => {
    expect(explainParams(' reed ', 'autoresearch:access', '  ')).toEqual({ login: 'reed', action: 'autoresearch:access' });
    expect(explainParams('reed', 'playbook:launch', ' pb-1 ')).toEqual({
      login: 'reed',
      action: 'playbook:launch',
      resource: 'pb-1',
    });
  });
});
