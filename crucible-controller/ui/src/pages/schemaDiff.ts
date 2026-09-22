import { parseParamsSchema, type ParamFieldSpec } from './playbookLaunchForm';

/// Which property of a param moved under a pin bump. The closed subset the form renders is the
/// whole vocabulary, so it is the whole diff.
export type ParamFieldKey = 'required' | 'pattern' | 'default' | 'description';

export interface ParamFieldChange {
  name: string;
  field: ParamFieldKey;
  from: string | null;
  to: string | null;
}

export interface SchemaDiff {
  added: ParamFieldSpec[];
  removed: ParamFieldSpec[];
  changed: ParamFieldChange[];
  /// True when neither schema renders as a form: nothing can be compared, so the bump has to be
  /// read as opaque rather than as "no change".
  comparable: boolean;
}

export const EMPTY_DIFF: SchemaDiff = { added: [], removed: [], changed: [], comparable: true };

export function diffIsEmpty(diff: SchemaDiff): boolean {
  return diff.added.length === 0 && diff.removed.length === 0 && diff.changed.length === 0;
}

function specsOf(schema: unknown): ParamFieldSpec[] | null {
  const parsed = parseParamsSchema(schema);
  return parsed.kind === 'form' ? parsed.specs : null;
}

function flagOf(spec: ParamFieldSpec, field: ParamFieldKey): string | null {
  switch (field) {
    case 'required':
      return spec.required ? 'required' : 'optional';
    case 'pattern':
      return spec.pattern;
    case 'default':
      return spec.defaultValue;
    case 'description':
      return spec.description;
  }
}

const FIELDS: readonly ParamFieldKey[] = ['required', 'pattern', 'default', 'description'];

/// What a pin bump does to the launch form: params gained, params lost, and every property that
/// moved on the ones that stayed. The registry's stored schema is `before`, the import preview's
/// is `after`; the whole comparison is client-side because both documents are already in hand.
export function diffParamsSchemas(before: unknown, after: unknown): SchemaDiff {
  const beforeSpecs = specsOf(before);
  const afterSpecs = specsOf(after);
  if (beforeSpecs === null || afterSpecs === null) {
    return { added: [], removed: [], changed: [], comparable: false };
  }
  const byName = new Map(beforeSpecs.map((spec) => [spec.name, spec]));
  const added: ParamFieldSpec[] = [];
  const changed: ParamFieldChange[] = [];
  for (const spec of afterSpecs) {
    const previous = byName.get(spec.name);
    if (previous === undefined) {
      added.push(spec);
      continue;
    }
    for (const field of FIELDS) {
      const from = flagOf(previous, field);
      const to = flagOf(spec, field);
      if (from !== to) changed.push({ name: spec.name, field, from, to });
    }
  }
  const kept = new Set(afterSpecs.map((spec) => spec.name));
  const removed = beforeSpecs.filter((spec) => !kept.has(spec.name));
  return { added, removed, changed, comparable: true };
}
