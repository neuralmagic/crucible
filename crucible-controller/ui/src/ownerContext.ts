import type { components } from './api/schema';

type Whoami = components['schemas']['Whoami'];
export type TeamRole = components['schemas']['TeamRole'];
export type Via = components['schemas']['Via'];

/// The context that shows everything the caller may read, rather than one owner's resources.
export const ALL = 'all';

export type PrincipalKind = 'user' | 'team' | 'group';

/// One principal the signed-in caller acts as, at the role they hold in it.
export interface ActedAs {
  /// `user:<login>`, `team:<slug>`, or `group:<path>`.
  value: string;
  label: string;
  kind: PrincipalKind;
  role: TeamRole;
}

const ROLE_RANK: Record<TeamRole, number> = { member: 0, maintainer: 1, owner: 2 };

/// Every principal the caller acts as: themselves, each team they reach at the role held, then
/// each group the credential proves. Empty for an anonymous caller.
export function actedAs(whoami: Whoami | undefined): ActedAs[] {
  if (whoami === undefined) return [];
  const login = whoami.user?.trim().toLowerCase() ?? '';
  const out: ActedAs[] = [];
  if (login.length > 0) {
    out.push({ value: `user:${login}`, label: login, kind: 'user', role: 'owner' });
  }
  const teams = [...whoami.teams].sort((a, b) => a.team.localeCompare(b.team));
  for (const team of teams) {
    out.push({ value: `team:${team.team}`, label: team.team, kind: 'team', role: team.role });
  }
  if (whoami.proves_groups) {
    for (const group of whoami.groups) {
      const path = group.trim().toLowerCase();
      if (path.length === 0) continue;
      const value = `group:${path}`;
      if (out.some((p) => p.value === value)) continue;
      out.push({ value, label: path, kind: 'group', role: 'owner' });
    }
  }
  return out;
}

/// The principals a new resource may be owned by: the caller, teams they maintain or own, and
/// groups. A team member cannot create under the team.
export function ownerOptions(principals: readonly ActedAs[]): ActedAs[] {
  return principals.filter((p) => p.kind !== 'team' || ROLE_RANK[p.role] >= ROLE_RANK.maintainer);
}

/// The principals the masthead switcher offers: the caller and each team.
export function switchable(principals: readonly ActedAs[]): ActedAs[] {
  return principals.filter((p) => p.kind !== 'group');
}

/// The stored context, or `all` when it names a principal the caller no longer acts as.
export function effectiveContext(stored: string | null, principals: readonly ActedAs[]): string {
  if (stored === null || stored === ALL) return ALL;
  return principals.some((p) => p.value === stored) ? stored : ALL;
}

/// Whether a resource with `owner` is in `context`.
export function inContext(context: string, owner: string | null | undefined): boolean {
  if (context === ALL) return true;
  return owner === context;
}

/// `rows` narrowed to the context, in their order.
export function narrow<T>(
  rows: readonly T[],
  context: string,
  ownerOf: (row: T) => string | null | undefined,
): T[] {
  if (context === ALL) return [...rows];
  return rows.filter((row) => inContext(context, ownerOf(row)));
}

/// The owner a creation form starts on: the context when it may own, else the first option.
export function defaultOwner(context: string, options: readonly ActedAs[]): string {
  if (options.some((o) => o.value === context)) return context;
  return options[0]?.value ?? '';
}

/// The owner a form actually sends: what it holds when the options offer it, else the default.
export function withOwner(current: string, context: string, options: readonly ActedAs[]): string {
  if (options.some((o) => o.value === current)) return current;
  return defaultOwner(context, options);
}

/// How one source holds a membership, as a settings row reads it.
export function viaLabel(via: Via): string {
  switch (via.kind) {
    case 'direct':
      return `direct · ${via.role}`;
    case 'group':
      return `group ${via.group} · ${via.role}`;
    case 'rule':
      return `rule ${via.rule} · ${via.role}`;
    case 'team':
      return `team ${via.team} · ${via.role}`;
  }
}
