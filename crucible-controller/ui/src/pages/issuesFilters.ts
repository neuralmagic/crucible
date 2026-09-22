// Pure helpers for the issues-page filter set: the upstream-recency presets, the input-kind,
// upstream-state and sort vocabularies (URL-seeded, so raw strings need honest parsing), and the
// repo split for the owner/name column.

import type { components } from '../api/schema.d';

type InputKindDto = components['schemas']['InputKindDto'];

export const RECENCY_PRESETS = ['7d', '30d', '90d', '1y', 'all'] as const;
export type RecencyPreset = (typeof RECENCY_PRESETS)[number];

/** Default: a year of upstream activity — ancient issues drown fresh ones, "all" is one click away. */
export const DEFAULT_RECENCY: RecencyPreset = '1y';

export const RECENCY_LABELS: Record<RecencyPreset, string> = {
  '7d': 'last 7 days',
  '30d': 'last 30 days',
  '90d': 'last 90 days',
  '1y': 'last year',
  all: 'all time',
};

const RECENCY_DAYS: Record<Exclude<RecencyPreset, 'all'>, number> = {
  '7d': 7,
  '30d': 30,
  '90d': 90,
  '1y': 365,
};

/** Parse a `?recency=` URL value; anything outside the vocabulary falls back to the default. */
export function parseRecency(raw: string | null): RecencyPreset {
  return RECENCY_PRESETS.find((p) => p === raw) ?? DEFAULT_RECENCY;
}

/**
 * The `upstream_since` API cutoff for a preset: a second-resolution RFC 3339 stamp (matching the
 * DB's stored format for clean lexical comparison), or undefined for `all`.
 */
export function recencyCutoff(preset: RecencyPreset, now: Date): string | undefined {
  if (preset === 'all') return undefined;
  const cutoff = new Date(now.getTime() - RECENCY_DAYS[preset] * 86_400_000);
  return cutoff.toISOString().replace(/\.\d{3}Z$/, 'Z');
}

// The input-kind filter vocabulary — the real, filterable kinds. The `?kind=` value is matched
// against the stored `input_kind` tag server-side, and that tag is exactly the DTO's `type`
// discriminant, so the vocabulary is derived from `InputKindDto` rather than retyped: a renamed or
// added kind is then a compile error here instead of a filter that silently matches nothing.
// `unknown` is a degraded catch-all with no stable tag to filter on, so it's excluded.
export type IssueKindFilter = Exclude<InputKindDto['type'], 'unknown'>;

// `satisfies` pins every entry to a real discriminant, so a misspelled or renamed kind fails to
// compile rather than becoming a filter value the server matches nothing against.
export const ISSUE_KINDS = [
  'github',
  'scenario',
  'jira',
  'playbook',
] as const satisfies readonly IssueKindFilter[];

// The Record pins the other direction: a newly added kind has to be labeled here. `ISSUE_KINDS`
// covering every key is checked in issuesFilters.test.ts (types can't see a missing tuple entry).
export const ISSUE_KIND_LABELS: Record<IssueKindFilter, string> = {
  github: 'GitHub',
  scenario: 'Scenario',
  jira: 'Jira',
  playbook: 'Playbook',
};

/** Parse a `?kind=` URL value; '' means no filter. */
export function parseKind(raw: string | null): IssueKindFilter | '' {
  return ISSUE_KINDS.find((k) => k === raw) ?? '';
}

export const UPSTREAM_STATES = ['open', 'closed'] as const;
export type UpstreamStateFilter = (typeof UPSTREAM_STATES)[number];

/** Parse a `?upstream=` URL value; '' means no filter. */
export function parseUpstream(raw: string | null): UpstreamStateFilter | '' {
  return UPSTREAM_STATES.find((s) => s === raw) ?? '';
}

// The `sort=` vocabulary `SortKey::parse` accepts (crucible-controller/src/model.rs). `updated`
// (our local touch stamp) stays valid on the wire but has no table column — upstream activity is
// what the page renders.
export const ISSUE_SORT_KEYS = ['updated', 'upstream', 'tier', 'priority', 'title'] as const;
export type IssueSortKey = (typeof ISSUE_SORT_KEYS)[number];

export const DEFAULT_SORT: IssueSortKey = 'upstream';

export function parseSortKey(raw: string | null): IssueSortKey {
  return ISSUE_SORT_KEYS.find((k) => k === raw) ?? DEFAULT_SORT;
}

export type SortDirection = 'asc' | 'desc';

export function parseSortDir(raw: string | null): SortDirection {
  return raw === 'asc' ? 'asc' : 'desc';
}

/** Split `owner/name` for the repo column; a slash-less value renders as name only. */
export function splitRepo(repo: string): { owner: string | null; name: string } {
  const idx = repo.indexOf('/');
  if (idx < 0) return { owner: null, name: repo };
  return { owner: repo.slice(0, idx), name: repo.slice(idx + 1) };
}
