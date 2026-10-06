import type { components } from '../api/schema';

type CatalogImageDto = components['schemas']['CatalogImageDto'];
type RankedImage = components['schemas']['RankedImage'];
type ExcludedImage = components['schemas']['ExcludedImage'];

export type BuildRow = Pick<CatalogImageDto, 'repository' | 'name' | 'digest' | 'tags' | 'created_at' | 'first_seen'>;

const COMMIT = /^[0-9a-f]{7,40}$/;

/// `latest`, else a named tag, else the short commit.
export function buildLabel(image: Pick<CatalogImageDto, 'tags'>): string {
  if (image.tags.includes('latest')) return 'latest';
  const named = image.tags.find((tag) => !COMMIT.test(tag));
  if (named !== undefined) return named;
  const commit = image.tags[0];
  return commit === undefined ? 'untagged' : commit.slice(0, 7);
}

/// `latest` first, then by created time, then first seen.
export function newestFirst(a: BuildRow, b: BuildRow): number {
  const latest = Number(b.tags.includes('latest')) - Number(a.tags.includes('latest'));
  if (latest !== 0) return latest;
  const created = (b.created_at ?? '').localeCompare(a.created_at ?? '');
  if (created !== 0) return created;
  return b.first_seen.localeCompare(a.first_seen) || a.digest.localeCompare(b.digest);
}

export interface Build<T extends BuildRow> {
  image: T;
  /// Why it cannot be picked; null when compatible.
  reason: string | null;
}

export interface ImageChoice<T extends BuildRow> {
  repository: string;
  name: string;
  /// Every catalogued build, newest first.
  builds: Build<T>[];
  /// The newest compatible build.
  newest: T;
  isDefault: boolean;
}

/// One choice per repository with a compatible build, in rank order.
export function imageChoices<T extends BuildRow>(
  compatible: readonly (Pick<RankedImage, 'default'> & { image: T })[],
  excluded: readonly (Pick<ExcludedImage, 'unsatisfied'> & { image: T })[],
  unverified: readonly T[],
): ImageChoice<T>[] {
  const order: string[] = [];
  const isDefault = new Set<string>();
  for (const ranked of compatible) {
    if (!order.includes(ranked.image.repository)) order.push(ranked.image.repository);
    if (ranked.default) isDefault.add(ranked.image.repository);
  }
  const all: Build<T>[] = [
    ...compatible.map((r) => ({ image: r.image, reason: null })),
    ...excluded.map((e) => ({ image: e.image, reason: exclusionReason(e.unsatisfied) })),
    ...unverified.map((image) => ({ image, reason: 'unverified' })),
  ];
  return order.flatMap((repository) => {
    const builds = all.filter((b) => b.image.repository === repository).sort((a, b) => newestFirst(a.image, b.image));
    const newest = builds.find((b) => b.reason === null)?.image;
    if (newest === undefined) return [];
    return [{ repository, name: newest.name, builds, newest, isDefault: isDefault.has(repository) }];
  });
}

export interface Unavailable {
  repository: string;
  name: string;
  reason: string;
}

export interface UnavailableGroup {
  reason: string;
  rows: Unavailable[];
}

/// Unavailable images that share a reason, the largest group first.
export function byReason(rows: readonly Unavailable[]): UnavailableGroup[] {
  const groups = new Map<string, Unavailable[]>();
  for (const row of rows) groups.set(row.reason, [...(groups.get(row.reason) ?? []), row]);
  return [...groups]
    .map(([reason, members]) => ({ reason, rows: members }))
    .sort((a, b) => b.rows.length - a.rows.length || a.reason.localeCompare(b.reason));
}

function exclusionReason(unsatisfied: ExcludedImage['unsatisfied']): string {
  return unsatisfied
    .map((u) =>
      u.found === null || u.found === undefined ? `lacks ${u.predicate}` : `${u.predicate} ${u.found} ≠ ${u.required}`,
    )
    .join(', ');
}

/// Repositories with no compatible build, with the reason from the newest one.
export function unavailableImages(
  excluded: readonly (Pick<ExcludedImage, 'unsatisfied'> & { image: BuildRow })[],
  unverified: readonly BuildRow[],
  choices: readonly Pick<ImageChoice<BuildRow>, 'repository'>[],
): Unavailable[] {
  const pickable = new Set(choices.map((c) => c.repository));
  const rows = [
    ...excluded.map((e) => ({ image: e.image, reason: exclusionReason(e.unsatisfied) })),
    ...unverified.map((image) => ({ image, reason: 'unverified' })),
  ].sort((a, b) => newestFirst(a.image, b.image));
  const seen = new Set<string>();
  const out: Unavailable[] = [];
  for (const { image, reason } of rows) {
    if (pickable.has(image.repository) || seen.has(image.repository)) continue;
    seen.add(image.repository);
    out.push({ repository: image.repository, name: image.name, reason });
  }
  return out.sort((a, b) => a.name.localeCompare(b.name));
}
