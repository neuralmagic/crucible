import { describe, expect, it } from 'vitest';
import {
  credentialView,
  keyView,
  mcpInstall,
  revokedMessage,
  type ApiKeyDto,
  type CredentialDto,
} from './settingsView';

function credential(over: Partial<CredentialDto> = {}): CredentialDto {
  return { present: true, refreshed_at: null, last_error: null, failures: 0, ...over };
}

describe('credentialView', () => {
  it('says nothing is stored, and offers nothing to revoke', () => {
    const view = credentialView(credential({ present: false }));
    expect(view.state).toBe('absent');
    expect(view.revocable).toBe(false);
    expect(view.detail).toMatch(/snapshot/);
  });

  it('surfaces the refusal verbatim once a refresh has failed', () => {
    const view = credentialView(
      credential({ last_error: 'the refresh was refused: invalid_grant', failures: 2 }),
    );
    expect(view.state).toBe('failing');
    expect(view.headline).toBe('NEEDS SIGN-IN');
    expect(view.detail).toBe('the refresh was refused: invalid_grant');
    expect(view.revocable).toBe(true);
  });

  it('distinguishes a credential that has been spent from one that has not', () => {
    expect(credentialView(credential()).detail).toMatch(/not used yet/);
    expect(credentialView(credential({ refreshed_at: '2026-08-24T10:00:00Z' })).detail).toContain(
      '2026-08-24T10:00:00Z',
    );
  });
});

describe('revokedMessage', () => {
  it('counts the schedules that stopped, and pluralizes them', () => {
    expect(revokedMessage(true, 0)).toBe('Offline credential revoked. No schedules needed it.');
    expect(revokedMessage(true, 1)).toContain('1 schedule will not fire');
    expect(revokedMessage(true, 3)).toContain('3 schedules will not fire');
  });

  it('says so when there was nothing to revoke', () => {
    expect(revokedMessage(false, 0)).toMatch(/nothing stored to revoke/);
  });
});

function apiKey(over: Partial<ApiKeyDto> = {}): ApiKeyDto {
  return {
    id: 'abc123',
    name: 'laptop',
    created_at: '2026-08-01T00:00:00Z',
    expires_at: null,
    last_used_at: null,
    revoked_at: null,
    ...over,
  };
}

describe('keyView', () => {
  const now = new Date('2026-08-28T00:00:00Z');

  it('is active and revocable while it lives, and says it never expires', () => {
    const view = keyView(apiKey(), now);
    expect(view.state).toBe('live');
    expect(view.revocable).toBe(true);
    expect(view.detail).toContain('Never used yet');
    expect(view.detail).toContain('Does not expire');
  });

  it('reports the last use once there is one', () => {
    const view = keyView(apiKey({ last_used_at: '2026-08-27T12:00:00Z' }), now);
    expect(view.detail).toContain('Last used 2026-08-27T12:00:00Z');
  });

  /// An expired key is not a revocable one: there is nothing left to stop.
  it('expires on the stamp passing, and offers no revoke', () => {
    const view = keyView(apiKey({ expires_at: '2026-08-27T00:00:00Z' }), now);
    expect(view.state).toBe('expired');
    expect(view.revocable).toBe(false);
    expect(view.detail).toContain('Mint a new one');
  });

  it('still counts as live on the boundary it has not yet crossed', () => {
    expect(keyView(apiKey({ expires_at: '2026-08-29T00:00:00Z' }), now).state).toBe('live');
  });

  /// Revocation wins over expiry: the owner's own act is the more useful thing to be told.
  it('reads as revoked even when the expiry has also passed', () => {
    const view = keyView(
      apiKey({ expires_at: '2026-08-01T00:00:00Z', revoked_at: '2026-08-02T00:00:00Z' }),
      now,
    );
    expect(view.state).toBe('revoked');
    expect(view.revocable).toBe(false);
  });
});

describe('mcpInstall', () => {
  it('produces a command carrying the secret and the url as given', () => {
    expect(mcpInstall('crk_abc_def', 'https://crucible-api.example.com/mcp')).toBe(
      'claude mcp add --transport http crucible https://crucible-api.example.com/mcp --header "Authorization: Bearer crk_abc_def"',
    );
  });
});
