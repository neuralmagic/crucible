import { CEDAR_KEYWORDS } from './cedar';
import type { CodeCompletion, CodeCompletions } from './CodeSurfaceImpl';

export interface CedarVocabulary {
  /// Every `Action::"..."` id the schema declares: `<resource>:<verb>` and the verb groups.
  actions: readonly string[];
  entityTypes: readonly string[];
}

export const TAG_PREFIXES = ['user:', 'group:', 'team:'] as const;

export function cedarVocabulary(
  actions: readonly { action: string; verb: string }[],
  schema: string,
): CedarVocabulary {
  const verbs = [...new Set(actions.map((a) => a.verb))].sort();
  return { actions: [...actions.map((a) => a.action), ...verbs], entityTypes: schemaEntityTypes(schema) };
}

/// The entity type names a Cedar schema declares, in order, `entity A, B { ... }` lists included.
export function schemaEntityTypes(schema: string): string[] {
  const names: string[] = [];
  const uncommented = schema.replace(/\/\/.*$/gm, '');
  for (const match of uncommented.matchAll(/\bentity\s+([A-Za-z_]\w*(?:\s*,\s*[A-Za-z_]\w*)*)/g)) {
    for (const name of match[1].split(',')) {
      const trimmed = name.trim();
      if (!names.includes(trimmed)) names.push(trimmed);
    }
  }
  return names;
}

/// Where the line prefix stands: inside a string opened at `open`, in a comment, or in code.
function lexicalState(prefix: string): { kind: 'code' } | { kind: 'comment' } | { kind: 'string'; open: number } {
  let i = 0;
  while (i < prefix.length) {
    const c = prefix[i];
    if (c === '/' && prefix[i + 1] === '/') return { kind: 'comment' };
    if (c === '"') {
      const open = i;
      i++;
      while (i < prefix.length && prefix[i] !== '"') i += prefix[i] === '\\' ? 2 : 1;
      if (i >= prefix.length) return { kind: 'string', open };
    }
    i++;
  }
  return { kind: 'code' };
}

function items(labels: readonly string[], kind: CodeCompletion['kind']): CodeCompletion[] {
  return labels.map((label) => ({ label, kind }));
}

export function cedarCompletions(
  prefix: string,
  vocabulary: CedarVocabulary,
): { from: number; items: CodeCompletion[] } | null {
  const state = lexicalState(prefix);
  if (state.kind === 'comment') return null;
  if (state.kind === 'string') {
    const before = prefix.slice(0, state.open);
    const from = state.open + 1;
    if (/\bAction\s*::\s*$/.test(before)) return { from, items: items(vocabulary.actions, 'value') };
    if (/\.\s*(?:hasTag|getTag)\s*\(\s*$/.test(before)) return { from, items: items(TAG_PREFIXES, 'value') };
    return null;
  }
  const word = /[A-Za-z_]\w*$/.exec(prefix)?.[0] ?? '';
  const from = prefix.length - word.length;
  const previous = prefix[from - 1];
  if (previous !== undefined && /[\w:."@]/.test(previous)) return null;
  return { from, items: [...items(vocabulary.entityTypes, 'type'), ...items(CEDAR_KEYWORDS, 'keyword')] };
}

export function cedarCompletionSource(vocabulary: CedarVocabulary): CodeCompletions {
  return {
    triggers: ['"'],
    complete: (prefix) => cedarCompletions(prefix, vocabulary),
  };
}
