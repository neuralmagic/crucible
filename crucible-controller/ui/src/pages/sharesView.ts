import type { components } from '../api/schema';

export type ShareDto = components['schemas']['ShareDto'];
export type ShareRole = components['schemas']['ShareRole'];

export const SHARE_ROLES: readonly { value: ShareRole; label: string }[] = [
  { value: 'viewer', label: 'viewer (read)' },
  { value: 'launcher', label: 'launcher (read, launch)' },
  { value: 'editor', label: 'editor (read, update, launch, bind)' },
];

export interface ShareForm {
  grantee: string;
  role: ShareRole;
  /// A date (`YYYY-MM-DD`) the share stops at, or blank for no expiry.
  until: string;
}

export function emptyShareForm(): ShareForm {
  return { grantee: '', role: 'viewer', until: '' };
}

/// Whether a grantee is spelled as a principal a share may name.
export function validGrantee(grantee: string): boolean {
  return /^(user|team):[^\s:]+$/.test(grantee.trim());
}

/// The `not_after` a form sends: the end of the chosen day in UTC, or null for none.
export function notAfter(until: string): string | null {
  const day = until.trim();
  if (day.length === 0) return null;
  return `${day}T23:59:59Z`;
}

/// Live shares first, then expired, each by grantee.
export function sortedShares(shares: readonly ShareDto[]): ShareDto[] {
  return [...shares].sort(
    (a, b) => Number(a.expired) - Number(b.expired) || a.grantee.localeCompare(b.grantee),
  );
}

/// What a share's expiry column reads.
export function expiryLabel(share: ShareDto): string {
  if (share.not_after === null || share.not_after === undefined) return 'never';
  return share.expired ? `expired ${share.not_after}` : share.not_after;
}
