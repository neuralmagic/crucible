import { describe, expect, it } from 'vitest';
import { consentPath, consentView, type ConsentDto } from './consentView';

function consent(identity: Partial<ConsentDto['identity']> = {}, client: Partial<ConsentDto> = {}): ConsentDto {
  return {
    id: 'abc',
    client_id: 'claude-code',
    client_name: 'Claude Code',
    client_host: null,
    resource: 'http://localhost:8851/mcp',
    scopes: ['kueue.read', 'kueue.submit'],
    login: 'jordan',
    identity: {
      cluster: 'pirate',
      rule: { kind: 'link', oauth: true },
      kube_user: null,
      source: null,
      linked_at: null,
      ...identity,
    },
    ...client,
  };
}

describe('consentView', () => {
  it('names the registered client asking by its display name', () => {
    const view = consentView(consent());
    expect(view.title).toBe('Allow Claude Code?');
    expect(view.client).toBe('Claude Code');
    expect(view.publisher).toBeNull();
    expect(consentView(consent({}, { client_id: 'codex', client_name: 'Codex' })).title).toBe('Allow Codex?');
  });

  it('names a metadata document client with the host that published it', () => {
    const view = consentView(
      consent(
        {},
        {
          client_id: 'https://chatgpt.com/oauth/codex/client.json',
          client_name: 'Codex',
          client_host: 'chatgpt.com',
        },
      ),
    );
    expect(view.title).toBe('Allow Codex (chatgpt.com)?');
    expect(view.client).toBe('Codex (chatgpt.com)');
    expect(view.publisher).toMatch(/^chatgpt\.com published this client/);
  });

  it('holds Allow back until a link cluster names the person', () => {
    const view = consentView(consent());
    expect(view.canApprove).toBe(false);
    expect(view.blocked).toBe('Link your username on pirate first.');
  });

  it('allows once the cluster names who the person acts as', () => {
    const view = consentView(consent({ kube_user: 'casey', source: 'openshift-oauth' }));
    expect(view.canApprove).toBe(true);
    expect(view.blocked).toBeNull();
  });

  it('says a claim cluster with no value is for an administrator, not a link', () => {
    const view = consentView(
      consent({ cluster: 'waldorf', rule: { kind: 'claim', claim: 'email', prefix: null } }),
    );
    expect(view.canApprove).toBe(false);
    expect(view.blocked).toMatch(/no email for waldorf/);
  });
});

describe('consentPath', () => {
  it('escapes the id', () => {
    expect(consentPath('a b&c')).toBe('/consent?id=a%20b%26c');
  });
});
