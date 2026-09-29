import { describe, expect, it } from 'vitest';
import { cedarCompletions, cedarVocabulary, schemaEntityTypes, type CedarVocabulary } from './cedarComplete';

const SCHEMA = `entity UserPrincipal { id: String } tags String;
// entity Commented { id: String };
entity Playbook, PlaybookDraft { owner: String };
entity Team { id: String };
action "read";
action "team:read" in [Action::"read"] appliesTo { principal: [UserPrincipal], resource: [Team] };
`;

const VOCABULARY: CedarVocabulary = {
  actions: ['autoresearch:access', 'playbook:launch', 'access', 'launch'],
  entityTypes: ['UserPrincipal', 'Team'],
};

function labels(prefix: string): string[] | null {
  return cedarCompletions(prefix, VOCABULARY)?.items.map((item) => item.label) ?? null;
}

describe('schemaEntityTypes', () => {
  it('reads every declared entity type, lists included, comments skipped', () => {
    expect(schemaEntityTypes(SCHEMA)).toEqual(['UserPrincipal', 'Playbook', 'PlaybookDraft', 'Team']);
  });

  it('is empty for no schema', () => {
    expect(schemaEntityTypes('')).toEqual([]);
  });
});

describe('cedarVocabulary', () => {
  it('offers every action and each verb group once', () => {
    const vocabulary = cedarVocabulary(
      [
        { action: 'playbook:launch', verb: 'launch' },
        { action: 'playbook:read', verb: 'read' },
        { action: 'team:read', verb: 'read' },
      ],
      SCHEMA,
    );
    expect(vocabulary.actions).toEqual(['playbook:launch', 'playbook:read', 'team:read', 'launch', 'read']);
    expect(vocabulary.entityTypes).toEqual(['UserPrincipal', 'Playbook', 'PlaybookDraft', 'Team']);
  });
});

describe('cedarCompletions', () => {
  it('offers actions inside an Action string, replacing from after the quote', () => {
    const prefix = 'permit(principal, action == Action::"auto';
    const found = cedarCompletions(prefix, VOCABULARY);
    expect(found?.from).toBe(prefix.length - 4);
    expect(found?.items.map((item) => item.label)).toEqual(VOCABULARY.actions);
    expect(found?.items.every((item) => item.kind === 'value')).toBe(true);
  });

  it('offers actions in an action list after earlier entries', () => {
    expect(labels('action in [Action::"read", Action::"')).toEqual(VOCABULARY.actions);
  });

  it('offers the tag prefixes inside hasTag and getTag', () => {
    expect(labels('when { principal.hasTag("')).toEqual(['user:', 'group:', 'team:']);
    expect(labels('when { principal.hasTag( "gr')).toEqual(['user:', 'group:', 'team:']);
    expect(labels('principal.getTag("')).toEqual(['user:', 'group:', 'team:']);
  });

  it('offers nothing inside any other string', () => {
    expect(labels('@id("')).toBeNull();
    expect(labels('Team::"')).toBeNull();
    expect(labels('resource.owner == "team:')).toBeNull();
  });

  it('knows a closed string from an open one, escapes included', () => {
    expect(labels('resource.owner == "a\\"b" && principal.hasTag("')).toEqual(['user:', 'group:', 'team:']);
    expect(labels('Action::"read" && ')).toContain('UserPrincipal');
  });

  it('offers nothing in a comment, even one that looks like code', () => {
    expect(labels('// Action::"')).toBeNull();
    expect(labels('permit(principal, action, resource); // Us')).toBeNull();
  });

  it('offers entity types and keywords for a word in code', () => {
    const prefix = 'permit(principal is Us';
    const found = cedarCompletions(prefix, VOCABULARY);
    expect(found?.from).toBe(prefix.length - 2);
    expect(found?.items.filter((item) => item.kind === 'type').map((item) => item.label)).toEqual(['UserPrincipal', 'Team']);
    expect(found?.items.map((item) => item.label)).toContain('permit');
    expect(labels('')).toContain('forbid');
  });

  it('offers nothing after an attribute dot, a path separator, or an annotation sign', () => {
    expect(labels('principal.pl')).toBeNull();
    expect(labels('Action::')).toBeNull();
    expect(labels('@i')).toBeNull();
    expect(labels('12ab')).toBeNull();
  });
});
