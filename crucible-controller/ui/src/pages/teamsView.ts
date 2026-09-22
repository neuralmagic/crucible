import type { components } from '../api/schema';

export type TeamDto = components['schemas']['TeamDto'];
export type MemberDto = components['schemas']['MemberDto'];
export type MemberKind = components['schemas']['MemberKind'];

/// How a row on a team's member list holds its membership.
export const HELD: Record<MemberKind, string> = {
  user: 'direct',
  group: 'group',
  team: 'nested team',
  rule: 'rule',
};

const ROLE_RANK = { owner: 0, maintainer: 1, member: 2 } as const;
const KIND_RANK: Record<MemberKind, number> = { user: 0, team: 1, group: 2, rule: 3 };

/// Members by role (owners first), then by how they are held, then by name.
export function sortedMembers(members: readonly MemberDto[]): MemberDto[] {
  return [...members].sort(
    (a, b) =>
      ROLE_RANK[a.role] - ROLE_RANK[b.role] ||
      KIND_RANK[a.kind] - KIND_RANK[b.kind] ||
      a.member.localeCompare(b.member),
  );
}

/// The teams the caller is in first, then the rest, each group by slug.
export function sortedTeams(teams: readonly TeamDto[]): TeamDto[] {
  return [...teams].sort(
    (a, b) =>
      Number(b.my_role !== null && b.my_role !== undefined) -
        Number(a.my_role !== null && a.my_role !== undefined) || a.slug.localeCompare(b.slug),
  );
}

/// Whether a slug is well formed: lowercase letters, digits, and dashes, starting with a letter.
export function validSlug(slug: string): boolean {
  return /^[a-z][a-z0-9-]{0,62}$/.test(slug);
}
