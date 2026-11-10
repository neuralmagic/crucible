import type { components } from '../api/schema.d';

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
