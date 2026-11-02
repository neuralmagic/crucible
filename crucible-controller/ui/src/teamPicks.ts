/// Which of the caller's teams the rail and the switcher name. Group claims can put a caller in
/// dozens of teams, so only the ones they starred or that own something are named; the rest wait
/// on the teams page.

/// How many unstarred active teams the rail names before deferring to the teams page.
export const ACTIVE_SHOWN = 6;

/// The teams that own at least one of the given resources, by slug.
export function activeTeams(owners: Iterable<string | null | undefined>): Set<string> {
  const out = new Set<string>();
  for (const owner of owners) {
    if (owner?.startsWith('team:')) out.add(owner.slice('team:'.length));
  }
  return out;
}

export interface TeamPick {
  slug: string;
  starred: boolean;
}

/// The teams to name, in order: starred ones, then up to `limit` active ones, then `current` when
/// neither list holds it. A caller in no more than `limit` teams has every one named, starred
/// first. `hidden` counts the memberships left unnamed.
export function pickTeams(
  teams: readonly string[],
  starred: readonly string[],
  active: ReadonlySet<string>,
  current: string | null,
  limit: number = ACTIVE_SHOWN,
): { shown: TeamPick[]; hidden: number } {
  const sorted = [...new Set(teams)].sort((a, b) => a.localeCompare(b));
  const stars = new Set(starred);
  const shown: TeamPick[] = sorted.filter((slug) => stars.has(slug)).map((slug) => ({ slug, starred: true }));
  if (sorted.length <= limit) {
    shown.push(...sorted.filter((slug) => !stars.has(slug)).map((slug) => ({ slug, starred: false })));
    return { shown, hidden: 0 };
  }
  shown.push(
    ...sorted
      .filter((slug) => !stars.has(slug) && active.has(slug))
      .slice(0, limit)
      .map((slug) => ({ slug, starred: false })),
  );
  if (current !== null && sorted.includes(current) && !shown.some((t) => t.slug === current)) {
    shown.push({ slug: current, starred: false });
  }
  return { shown, hidden: sorted.length - shown.length };
}

/// Whether a team matches a filter typed on the teams page: a case-insensitive substring of its
/// slug or its name.
export function matchesTeam(query: string, slug: string, name: string): boolean {
  const q = query.trim().toLowerCase();
  if (q.length === 0) return true;
  return slug.toLowerCase().includes(q) || name.toLowerCase().includes(q);
}
