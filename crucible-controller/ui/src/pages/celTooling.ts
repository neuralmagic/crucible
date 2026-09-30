import type { components } from '../api/schema.d';
import type { CodeCompletion, CodeCompletions, CodeMarker } from '../editor/CodeSurface';

export type CelLanguageDto = components['schemas']['CelLanguageDto'];
export type CelDiagnosticDto = components['schemas']['CelDiagnosticDto'];

function item(label: string, kind: CodeCompletion['kind']): CodeCompletion {
  return { label, kind };
}

/// What to offer at the caret: after a `.`, the functions and macros called on a target; anywhere
/// else, the variables, the global functions, and `has`.
export function celCompletions(language: CelLanguageDto): CodeCompletions {
  const member: CodeCompletion[] = [
    ...language.functions.filter((f) => f.member).map((f) => item(f.name, 'value')),
    ...language.macros.filter((m) => m !== 'has').map((m) => item(m, 'keyword')),
  ];
  const global: CodeCompletion[] = [
    ...language.variables.map((v) => item(v, 'type')),
    ...language.functions.filter((f) => !f.member).map((f) => item(f.name, 'value')),
    ...language.macros.filter((m) => m === 'has').map((m) => item(m, 'keyword')),
  ];
  return {
    triggers: ['.'],
    complete: (linePrefix) => {
      const word = /[A-Za-z_]\w*$/.exec(linePrefix);
      const from = word === null ? linePrefix.length : word.index;
      const typed = word === null ? '' : word[0];
      const items = linePrefix.slice(0, from).endsWith('.') ? member : global;
      return { from, items: items.filter((candidate) => candidate.label.startsWith(typed)) };
    },
  };
}

/// A field's diagnostics as editor markers. One without a position covers the whole expression.
export function markersOf(diagnostics: readonly CelDiagnosticDto[], source: string): CodeMarker[] {
  const lines = source.split('\n');
  const lastLine = Math.max(lines.length, 1);
  const lastCol = (lines[lines.length - 1] ?? '').length + 1;
  return diagnostics.map((d) =>
    d.line != null && d.column != null
      ? { line: d.line, col: d.column, message: d.message }
      : { line: 1, col: 1, endLine: lastLine, endCol: Math.max(lastCol, 2), message: d.message }
  );
}

/// Diagnostics grouped by field: `filter`, `dedupe`, `derive.<param>`.
export function byField(diagnostics: readonly CelDiagnosticDto[]): ReadonlyMap<string, CelDiagnosticDto[]> {
  const grouped = new Map<string, CelDiagnosticDto[]>();
  for (const d of diagnostics) {
    grouped.set(d.field, [...(grouped.get(d.field) ?? []), d]);
  }
  return grouped;
}
