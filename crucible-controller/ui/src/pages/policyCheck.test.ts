import { describe, expect, it } from 'vitest';
import { validate, type ValidationAnswer } from '@cedar-policy/cedar-wasm/nodejs';
import type { CodeMarker } from '../editor/CodeSurface';
import { idMarkers, policyMarkers, positionAt, utf16Index } from './policyCheck';

const SCHEMA = `entity UserPrincipal { id: String, platform_admin: Bool, proves_groups: Bool } tags String;
entity TeamPrincipal { id: String, platform_admin: Bool, proves_groups: Bool } tags String;
entity RunPrincipal { id: String, platform_admin: Bool, proves_groups: Bool } tags String;
entity Autoresearch { id: String, owner: String, owner_kind: String, owner_role?: String, share?: String, run?: String };
action "access";
action "autoresearch:access" in [Action::"access"] appliesTo { principal: [UserPrincipal, TeamPrincipal, RunPrincipal], resource: [Autoresearch], context: { now: Long } };
`;

function check(text: string): CodeMarker[] {
  return policyMarkers(validate({ schema: SCHEMA, policies: { staticPolicies: text }, validationSettings: { mode: 'strict' } }), text);
}

/// The text a marker covers, read back through the same line/column addressing Monaco uses.
function covered(text: string, marker: CodeMarker): string {
  const lines = text.split('\n');
  const endLine = marker.endLine ?? marker.line;
  const endCol = marker.endCol ?? lines[marker.line - 1].length + 1;
  if (endLine === marker.line) return lines[marker.line - 1].slice(marker.col - 1, endCol - 1);
  return [
    lines[marker.line - 1].slice(marker.col - 1),
    ...lines.slice(marker.line, endLine - 1),
    lines[endLine - 1].slice(0, endCol - 1),
  ].join('\n');
}

describe('utf16Index', () => {
  it('is the identity over ASCII', () => {
    expect(utf16Index('permit;', 0)).toBe(0);
    expect(utf16Index('permit;', 6)).toBe(6);
  });

  it('counts two-byte, three-byte and four-byte characters as the units JavaScript holds them in', () => {
    const text = 'é€🎉x';
    expect(utf16Index(text, 2)).toBe(1);
    expect(utf16Index(text, 5)).toBe(2);
    expect(utf16Index(text, 9)).toBe(4);
    expect(text[utf16Index(text, 9)]).toBe('x');
  });

  it('resolves an offset inside a character to that character', () => {
    expect(utf16Index('é🎉', 1)).toBe(0);
    expect(utf16Index('é🎉', 4)).toBe(1);
  });

  it('clamps an offset past the end to the length', () => {
    expect(utf16Index('ab', 10)).toBe(2);
    expect(utf16Index('', 0)).toBe(0);
  });
});

describe('positionAt', () => {
  it('addresses by 1-based line and column', () => {
    const text = 'ab\ncd\n\nef';
    expect(positionAt(text, 0)).toEqual({ line: 1, col: 1 });
    expect(positionAt(text, 2)).toEqual({ line: 1, col: 3 });
    expect(positionAt(text, 3)).toEqual({ line: 2, col: 1 });
    expect(positionAt(text, 7)).toEqual({ line: 4, col: 1 });
    expect(positionAt(text, 9)).toEqual({ line: 4, col: 3 });
  });

  it('clamps past the end', () => {
    expect(positionAt('ab', 50)).toEqual({ line: 1, col: 3 });
  });
});

describe('policyMarkers against the real validator', () => {
  it('marks nothing on a set that validates', () => {
    expect(
      check(
        '@id("ops")\npermit(principal is UserPrincipal, action == Action::"autoresearch:access", resource)\nwhen { principal.hasTag("team:platform-operators") };\n',
      ),
    ).toEqual([]);
  });

  it('marks nothing for a warning, since the server stores a set that only warns', () => {
    expect(check('@id("never")\npermit(principal, action, resource) when { 1 == 2 };\n')).toEqual([]);
  });

  it('puts a parse error on the exact token after multi-byte text', () => {
    const text = '// é🎉 owners\n@id("a")\npermit(principal, action, resource);\n@id("b")\nbogus;\n';
    const markers = check(text);
    expect(markers).toHaveLength(1);
    expect(markers[0]).toMatchObject({ line: 5, col: 6, endLine: 5, endCol: 7 });
    expect(covered(text, markers[0])).toBe(';');
    expect(markers[0].message).toContain('unexpected token `;`');
    expect(markers[0].message).toContain('expected `(`');
  });

  it('marks every parse error, including the ones Cedar nests as related', () => {
    const text = '@id("a") permit(principal, action, resource) when { 1 + };\n@id("b") permit(principal, action, resource) when { ) };\n';
    const markers = check(text);
    expect(markers.map((m) => covered(text, m))).toEqual(['}', ')']);
    expect(markers.map((m) => m.line)).toEqual([1, 2]);
  });

  it('spans an unknown action and carries the did-you-mean', () => {
    const text = '// ünïcode\n@id("a")\npermit(principal, action == Action::"autoresearch:acess", resource);\n';
    const markers = check(text);
    expect(markers).toHaveLength(1);
    expect(covered(text, markers[0])).toBe('Action::"autoresearch:acess"');
    expect(markers[0].message).toContain('did you mean `Action::"autoresearch:access"`?');
  });

  it('marks each type error at its own expression', () => {
    const text = '@id("a")\npermit(principal is UserPrincipal, action == Action::"autoresearch:access", resource)\nwhen { context.now > "x" && principal.nope };\n';
    const spans = check(text).map((m) => covered(text, m));
    expect(spans).toHaveLength(2);
    expect(spans).toContain('"x"');
    expect(spans).toContain('principal.nope');
  });

  it('spans a policy across lines', () => {
    const answer: ValidationAnswer = {
      type: 'success',
      validationErrors: [
        {
          policyId: 'policy0',
          error: {
            message: 'whole policy',
            help: null,
            code: null,
            url: null,
            severity: null,
            sourceLocations: [{ label: null, start: 0, end: 20 }],
          },
        },
      ],
      validationWarnings: [],
      otherWarnings: [],
    };
    const text = '@id("a")\npermit(p, a, r);\n';
    const [marker] = policyMarkers(answer, text);
    expect(marker).toMatchObject({ line: 1, col: 1, endLine: 2, endCol: 12 });
    expect(covered(text, marker)).toBe('@id("a")\npermit(p, a');
  });

  it('puts an error Cedar gives no location on the first line', () => {
    const answer: ValidationAnswer = {
      type: 'failure',
      errors: [{ message: 'schema does not parse', help: 'check the schema', code: null, url: null, severity: null }],
      warnings: [],
    };
    expect(policyMarkers(answer, 'x')).toEqual([{ line: 1, col: 1, message: 'schema does not parse\ncheck the schema' }]);
  });

  it('refuses what the server refuses for its @id once the set validates', () => {
    const text =
      '@id("a")\npermit(principal, action, resource);\n\npermit(principal, action, resource);\n@id("a")\nforbid(principal, action, resource);\n';
    expect(check(text)).toEqual([
      { line: 4, col: 1, message: 'policy carries no @id annotation' },
      { line: 5, col: 1, message: 'two policies carry @id a' },
    ]);
  });
});

describe('idMarkers', () => {
  it('passes a set whose every statement carries a distinct @id', () => {
    expect(idMarkers('// c\n@id("a") permit(principal, action, resource);\n@id("b") forbid(principal, action, resource);')).toEqual([]);
  });
});
