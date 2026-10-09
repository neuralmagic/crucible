import { describe, expect, it } from 'vitest';
import { consentPath, consentView, type ConsentDto } from './consentView';

function consent(identity: Partial<ConsentDto['identity']> = {}): ConsentDto {
  return {
    id: 'abc',
    client_id: 'claude-code',
    resource: 'http://localhost:8851/mcp',
    scopes: ['kueue.read', 'kueue.submit'],
    login: 'weaton',
    identity: {
      cluster: 'pirate',
      rule: { kind: 'link', oauth: true },
      kube_user: null,
      source: null,
      linked_at: null,
      ...identity,
    },
  };
}

describe('consentView', () => {
  it('names the client asking', () => {
    expect(consentView(consent()).title).toBe('Allow claude-code?');
  });

  it('holds Allow back until a link cluster names the person', () => {
    const view = consentView(consent());
    expect(view.canApprove).toBe(false);
    expect(view.blocked).toBe('Link your username on pirate first.');
  });

  it('allows once the cluster names who the person acts as', () => {
    const view = consentView(consent({ kube_user: 'wseaton', source: 'openshift-oauth' }));
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
