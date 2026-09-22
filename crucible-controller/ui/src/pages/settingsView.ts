import type { components } from '../api/schema.d';

export type CredentialDto = components['schemas']['CredentialDto'];

export type CredentialState = 'absent' | 'failing' | 'live';

export interface CredentialView {
  state: CredentialState;
  headline: string;
  detail: string;
  /** Whether there is anything for the revoke control to act on. */
  revocable: boolean;
}

/// What the settings panel says about the caller's offline credential. `failing` is a credential
/// that exists but was last refused: the schedules that need it are already parking, and signing in
/// again is the fix.
export function credentialView(credential: CredentialDto): CredentialView {
  if (!credential.present) {
    return {
      state: 'absent',
      headline: 'NONE STORED',
      detail:
        'Scheduled launches under team secrets fall back to the group snapshot your last save took, and park once it ages out. Sign in again to store one.',
      revocable: false,
    };
  }
  if (credential.last_error) {
    return {
      state: 'failing',
      headline: 'NEEDS SIGN-IN',
      detail: credential.last_error,
      revocable: true,
    };
  }
  return {
    state: 'live',
    headline: 'ACTIVE',
    detail: credential.refreshed_at
      ? `Last used ${credential.refreshed_at}.`
      : 'Stored at your last sign-in; not used yet.',
    revocable: true,
  };
}

/// The sentence the revoke result puts on screen: what went away, and what stopped with it.
export function revokedMessage(revoked: boolean, schedulesParked: number): string {
  const credential = revoked ? 'Offline credential revoked.' : 'There was nothing stored to revoke.';
  if (schedulesParked === 0) return `${credential} No schedules needed it.`;
  const plural = schedulesParked === 1 ? 'schedule' : 'schedules';
  return `${credential} ${schedulesParked} ${plural} will not fire until you sign in again.`;
}

export type ApiKeyDto = components['schemas']['ApiKey'];

export type KeyState = 'live' | 'expired' | 'revoked';

export interface KeyView {
  state: KeyState;
  /** What the status chip reads. */
  headline: string;
  /** The line under the name: what this key is doing, or why it stopped. */
  detail: string;
  /** Whether the revoke control has anything to act on. */
  revocable: boolean;
}

/// What one key's row says. A revoked key stays listed rather than vanishing, because "I revoked
/// that" is the answer to "why did my agent stop working", and a list that silently shortened
/// would not give it.
export function keyView(key: ApiKeyDto, now: Date = new Date()): KeyView {
  if (key.revoked_at) {
    return {
      state: 'revoked',
      headline: 'REVOKED',
      detail: `Revoked ${key.revoked_at}. Anything still presenting it is being refused.`,
      revocable: false,
    };
  }
  if (key.expires_at && new Date(key.expires_at) <= now) {
    return {
      state: 'expired',
      headline: 'EXPIRED',
      detail: `Expired ${key.expires_at}. Mint a new one to replace it.`,
      revocable: false,
    };
  }
  const used = key.last_used_at ? `Last used ${key.last_used_at}.` : 'Never used yet.';
  const until = key.expires_at ? ` Expires ${key.expires_at}.` : ' Does not expire.';
  return { state: 'live', headline: 'ACTIVE', detail: `${used}${until}`, revocable: true };
}

/// How to point an MCP client at this controller. Shown once beside a freshly minted key, because
/// that is the only moment the secret exists to paste into it.
export function mcpInstall(secret: string, mcpUrl: string): string {
  return `claude mcp add --transport http crucible ${mcpUrl} --header "Authorization: Bearer ${secret}"`;
}
