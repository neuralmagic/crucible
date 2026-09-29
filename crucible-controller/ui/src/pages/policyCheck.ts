import type { DetailedError, ValidationAnswer } from '@cedar-policy/cedar-wasm/web';
import type { CodeMarker } from '../editor/CodeSurface';
import { parsePolicies } from './policyView';

/// The UTF-16 index of a UTF-8 byte offset into `text`. Cedar reports byte offsets; an offset that
/// falls inside a character resolves to that character's start.
export function utf16Index(text: string, byteOffset: number): number {
  let bytes = 0;
  let i = 0;
  while (i < text.length) {
    const point = text.codePointAt(i) ?? 0;
    const width = point < 0x80 ? 1 : point < 0x800 ? 2 : point < 0x10000 ? 3 : 4;
    if (bytes + width > byteOffset) return i;
    bytes += width;
    i += point > 0xffff ? 2 : 1;
  }
  return text.length;
}

/// The 1-based line and column Monaco addresses a UTF-16 index by.
export function positionAt(text: string, index: number): { line: number; col: number } {
  let line = 1;
  let lineStart = 0;
  for (let i = 0; i < index && i < text.length; i++) {
    if (text[i] === '\n') {
      line++;
      lineStart = i + 1;
    }
  }
  return { line, col: Math.min(index, text.length) - lineStart + 1 };
}

function flatten(errors: readonly DetailedError[]): DetailedError[] {
  return errors.flatMap((error) => [error, ...flatten(error.related ?? [])]);
}

function marker(text: string, error: DetailedError): CodeMarker {
  const at = error.sourceLocations?.[0];
  const message = [error.message, at?.label ?? null, error.help].filter((part) => part !== null).join('\n');
  if (at === undefined) return { line: 1, col: 1, message };
  const start = positionAt(text, utf16Index(text, at.start));
  const end = positionAt(text, utf16Index(text, at.end));
  return { line: start.line, col: start.col, endLine: end.line, endCol: end.col, message };
}

/// Every statement the server refuses for its `@id`: none, or one another statement already holds.
export function idMarkers(text: string): CodeMarker[] {
  const seen = new Set<string>();
  const markers: CodeMarker[] = [];
  for (const rule of parsePolicies(text)) {
    if (rule.id === null) {
      markers.push({ line: rule.line, col: 1, message: 'policy carries no @id annotation' });
    } else if (seen.has(rule.id)) {
      markers.push({ line: rule.line, col: 1, message: `two policies carry @id ${rule.id}` });
    } else {
      seen.add(rule.id);
    }
  }
  return markers;
}

/// The markers for a policy set, one per error the validator reports. Warnings are left out: the
/// server stores a set that only warns.
export function policyMarkers(answer: ValidationAnswer, text: string): CodeMarker[] {
  if (answer.type === 'failure') return flatten(answer.errors).map((error) => marker(text, error));
  const invalid = flatten(answer.validationErrors.map((e) => e.error)).map((error) => marker(text, error));
  return [...invalid, ...idMarkers(text)];
}
