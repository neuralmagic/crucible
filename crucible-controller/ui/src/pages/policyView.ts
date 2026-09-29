import type { components } from '../api/schema';

export type PolicySetDto = components['schemas']['PolicySetDto'];
export type ActionDto = components['schemas']['ActionDto'];
export type ExplanationDto = components['schemas']['ExplanationDto'];

export type Effect = 'permit' | 'forbid';

/// One policy statement of a Cedar set, as written.
export interface PolicyRule {
  /// The `@id` annotation, or null when the statement carries none.
  id: string | null;
  effect: Effect | null;
  /// From the first annotation or effect keyword through the closing `;`.
  text: string;
  /// 1-based line the statement starts on.
  line: number;
}

function lineAt(text: string, offset: number): number {
  let line = 1;
  for (let i = 0; i < offset; i++) if (text[i] === '\n') line++;
  return line;
}

function statement(text: string, start: number, end: number): PolicyRule {
  const body = text.slice(start, end);
  const id = /@id\s*\(\s*"((?:[^"\\]|\\.)*)"\s*\)/.exec(body);
  const effect = /^(?:\s*@\w+\s*\(\s*"(?:[^"\\]|\\.)*"\s*\))*\s*(permit|forbid)\b/.exec(body);
  return {
    id: id === null ? null : id[1].replace(/\\(.)/g, '$1'),
    effect: effect === null ? null : effect[1] === 'permit' ? 'permit' : 'forbid',
    text: body,
    line: lineAt(text, start),
  };
}

/// Split Cedar source into its statements. A statement ends at a `;` outside a string literal or
/// a `//` comment; comments between statements belong to none of them.
export function parsePolicies(text: string): PolicyRule[] {
  const rules: PolicyRule[] = [];
  let start: number | null = null;
  let i = 0;
  while (i < text.length) {
    const c = text[i];
    if (c === '/' && text[i + 1] === '/') {
      const eol = text.indexOf('\n', i);
      i = eol === -1 ? text.length : eol + 1;
      continue;
    }
    if (c === '"') {
      start ??= i;
      i++;
      while (i < text.length && text[i] !== '"') i += text[i] === '\\' ? 2 : 1;
      i++;
      continue;
    }
    if (c === ';') {
      if (start !== null) rules.push(statement(text, start, i + 1));
      start = null;
    } else if (!/\s/.test(c)) {
      start ??= i;
    }
    i++;
  }
  if (start !== null) rules.push(statement(text, start, text.length));
  return rules;
}

export interface RuleDiff {
  added: string[];
  removed: string[];
  changed: string[];
}

function keyed(rules: readonly PolicyRule[]): Map<string, string> {
  return new Map(rules.map((rule) => [rule.id ?? rule.text, rule.text]));
}

/// What moving from `before` to `after` does, rule by rule, keyed on `@id`. A statement without
/// an id is keyed on its text, so editing one reads as a removal and an addition.
export function diffRules(before: readonly PolicyRule[], after: readonly PolicyRule[]): RuleDiff {
  const was = keyed(before);
  const now = keyed(after);
  const added: string[] = [];
  const changed: string[] = [];
  for (const [key, text] of now) {
    const prior = was.get(key);
    if (prior === undefined) added.push(key);
    else if (prior !== text) changed.push(key);
  }
  const removed = [...was.keys()].filter((key) => !now.has(key));
  return { added, removed, changed };
}

export function ruleDiffIsEmpty(diff: RuleDiff): boolean {
  return diff.added.length === 0 && diff.removed.length === 0 && diff.changed.length === 0;
}

/// Every stored set, the most recently created first.
export function newestFirst(sets: readonly PolicySetDto[]): PolicySetDto[] {
  return [...sets].sort((a, b) => b.created_at.localeCompare(a.created_at) || a.digest.localeCompare(b.digest));
}

export function shortDigest(digest: string): string {
  return digest.slice(0, 12);
}

/// The DOM id a rule of the active set is rendered under, so a decision can point at it.
export function ruleAnchor(id: string): string {
  return `rule-${id.replace(/[^A-Za-z0-9_-]/g, '_')}`;
}

export interface ExplainParams {
  login: string;
  action: string;
  resource?: string;
}

/// The explain query for a form, or null while it cannot be asked.
export function explainParams(login: string, action: string, resource: string): ExplainParams | null {
  const who = login.trim();
  if (who.length === 0 || action.length === 0) return null;
  const id = resource.trim();
  return id.length === 0 ? { login: who, action } : { login: who, action, resource: id };
}
