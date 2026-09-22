import type { components } from '../api/schema';
import type { MemberDto, MemberKind } from './teamsView';

export type MemberBody = components['schemas']['MemberBody'];
export type TeamRole = components['schemas']['TeamRole'];

export const MEMBER_KINDS: readonly { value: MemberKind; label: string }[] = [
  { value: 'user', label: 'user (a login)' },
  { value: 'group', label: 'group (a claim path)' },
  { value: 'team', label: 'team (a nested team)' },
  { value: 'rule', label: 'rule (email-domain:, group-prefix:, group-suffix:)' },
];

export const TEAM_ROLES: readonly { value: TeamRole; label: string }[] = [
  { value: 'member', label: 'member' },
  { value: 'maintainer', label: 'maintainer' },
  { value: 'owner', label: 'owner' },
];

/// The members as a PUT sends them.
export function asBodies(members: readonly MemberDto[]): MemberBody[] {
  return members.map((m) => ({ kind: m.kind, member: m.member, role: m.role }));
}

/// `members` with one added; a row naming the same kind and member is replaced.
export function withMember(members: readonly MemberBody[], added: MemberBody): MemberBody[] {
  const rest = members.filter((m) => !(m.kind === added.kind && m.member === added.member));
  return [...rest, { ...added, member: added.member.trim() }];
}

export function withoutMember(members: readonly MemberBody[], kind: MemberKind, member: string): MemberBody[] {
  return members.filter((m) => !(m.kind === kind && m.member === member));
}

export function withRole(
  members: readonly MemberBody[],
  kind: MemberKind,
  member: string,
  role: TeamRole,
): MemberBody[] {
  return members.map((m) => (m.kind === kind && m.member === member ? { ...m, role } : m));
}

/// A team keeps at least one user at owner; a list that drops the last one cannot be sent.
export function keepsAnOwner(members: readonly MemberBody[]): boolean {
  return members.some((m) => m.kind === 'user' && m.role === 'owner');
}
