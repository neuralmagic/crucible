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

/// What a member row offers a viewer who may view as other users: nothing for themselves or a
/// non-user row, a disabled button for a login that has never signed in, a live one otherwise.
export type ViewAsOffer = 'none' | 'never-signed-in' | 'ready';

export function viewAsOffer(member: MemberDto, viewer: string | null): ViewAsOffer {
  if (viewer === null || member.kind !== 'user' || member.member === viewer) return 'none';
  return member.signed_in ? 'ready' : 'never-signed-in';
}

/// Starred teams first, then the ones the caller is in, then the ones that own something, each
/// group by slug.
export function sortedTeams(
  teams: readonly TeamDto[],
  starred: ReadonlySet<string> = new Set(),
  active: ReadonlySet<string> = new Set(),
): TeamDto[] {
  const mine = (t: TeamDto) => t.my_role !== null && t.my_role !== undefined;
  return [...teams].sort(
    (a, b) =>
      Number(starred.has(b.slug)) - Number(starred.has(a.slug)) ||
      Number(mine(b)) - Number(mine(a)) ||
      Number(active.has(b.slug)) - Number(active.has(a.slug)) ||
      a.slug.localeCompare(b.slug),
  );
}

/// Whether a slug is well formed: lowercase letters, digits, and dashes, starting with a letter.
export function validSlug(slug: string): boolean {
  return /^[a-z][a-z0-9-]{0,62}$/.test(slug);
}
