import { describe, expect, it } from 'vitest';
import {
  clusterIdentitiesShown,
  identityView,
  linkOutcome,
  ruleLabel,
  sortIdentities,
  type ClusterIdentityDto,
} from './clusterIdentitiesView';

function linkCluster(over: Partial<ClusterIdentityDto> = {}): ClusterIdentityDto {
  return {
    cluster: 'pirate',
    rule: { kind: 'link', oauth: true },
    kube_user: null,
    source: null,
    linked_at: null,
    ...over,
  };
}

function ruleCluster(over: Partial<ClusterIdentityDto> = {}): ClusterIdentityDto {
  return {
    cluster: 'mpp',
    rule: { kind: 'claim', claim: 'login', prefix: null },
    kube_user: 'weaton',
    source: 'rule',
    linked_at: null,
    ...over,
  };
}

describe('ruleLabel', () => {
  it('names the claim and any prefix for a rule cluster', () => {
    expect(ruleLabel({ kind: 'claim', claim: 'login', prefix: null })).toBe('SSO login');
    expect(ruleLabel({ kind: 'claim', claim: 'email', prefix: 'oidc:' })).toBe(
      'SSO email, prefixed oidc:',
    );
  });

  it('says how a link cluster is proven', () => {
    expect(ruleLabel({ kind: 'link', oauth: true })).toBe('Linked, OpenShift sign-in');
    expect(ruleLabel({ kind: 'link', oauth: false })).toBe('Linked, pasted token');
  });
});

describe('identityView', () => {
  it('shows a rule cluster read-only, with the name the SSO login resolves to', () => {
    const view = identityView(ruleCluster());
    expect(view.state).toBe('resolved');
    expect(view.username).toBe('weaton');
    expect(view.link).toBeNull();
    expect(view.unlinkable).toBe(false);
  });

  it('says so when the SSO login carries nothing for the claim', () => {
    const view = identityView(
      ruleCluster({ rule: { kind: 'claim', claim: 'email', prefix: null }, kube_user: null, source: null }),
    );
    expect(view.username).toBe('—');
    expect(view.detail).toMatch(/carries no value/);
    expect(view.link).toBeNull();
  });

  it('offers an OpenShift sign-in on an unlinked OAuth cluster', () => {
    const view = identityView(linkCluster());
    expect(view.state).toBe('unlinked');
    expect(view.link).toEqual({ method: 'oauth', label: 'Link' });
    expect(view.unlinkable).toBe(false);
  });

  it('falls back to pasting a token where the cluster has no OAuth server', () => {
    const view = identityView(linkCluster({ rule: { kind: 'link', oauth: false } }));
    expect(view.link).toEqual({ method: 'paste', label: 'Link' });
    expect(view.detail).toMatch(/never stored/);
  });

  it('shows who a linked cluster says you are, when, and how, and offers relink and unlink', () => {
    const view = identityView(
      linkCluster({
        kube_user: 'wseaton',
        source: 'openshift-oauth',
        linked_at: '2026-10-08T20:00:00Z',
      }),
    );
    expect(view.state).toBe('linked');
    expect(view.username).toBe('wseaton');
    expect(view.detail).toBe('Proven by signing in to the cluster on 2026-10-08T20:00:00Z.');
    expect(view.link).toEqual({ method: 'oauth', label: 'Relink' });
    expect(view.unlinkable).toBe(true);
  });

  it('names a pasted token as the proof when that is how the link was made', () => {
    const view = identityView(
      linkCluster({ kube_user: 'wseaton', source: 'token-review', linked_at: '2026-10-08T20:00:00Z' }),
    );
    expect(view.detail).toContain('TokenReview');
  });

  /// The person cannot undo or replace an administrator's decision from their own settings.
  it('offers no control over an administrator override, on either kind of cluster', () => {
    for (const identity of [
      linkCluster({ kube_user: 'alice-admin', source: 'override' }),
      ruleCluster({ kube_user: 'alice-admin', source: 'override' }),
    ]) {
      const view = identityView(identity);
      expect(view.state).toBe('override');
      expect(view.link).toBeNull();
      expect(view.unlinkable).toBe(false);
      expect(view.detail).toMatch(/administrator/);
    }
  });
});

describe('sortIdentities', () => {
  it('puts clusters waiting on a link first, then orders by name', () => {
    const sorted = sortIdentities([
      ruleCluster({ cluster: 'waldorf' }),
      linkCluster({ cluster: 'zeta', kube_user: 'z', source: 'token-review' }),
      ruleCluster({ cluster: 'mpp' }),
      linkCluster({ cluster: 'pirate' }),
    ]);
    expect(sorted.map((i) => i.cluster)).toEqual(['pirate', 'mpp', 'waldorf', 'zeta']);
  });
});

describe('linkOutcome', () => {
  it('reports the cluster a link just completed for', () => {
    expect(linkOutcome(new URLSearchParams('linked=pirate'))).toEqual({ ok: true, cluster: 'pirate' });
  });

  it('maps a known error code to its sentence', () => {
    expect(linkOutcome(new URLSearchParams('link_error=state-mismatch'))).toEqual({
      ok: false,
      message: 'The sign-in did not match the link this browser started. Start it again.',
    });
  });

  /// The query string is attacker-controllable; only fixed sentences ever reach the page.
  it('never echoes an unknown code', () => {
    const outcome = linkOutcome(new URLSearchParams('link_error=Call%20555-0100%20for%20support'));
    expect(outcome).toEqual({ ok: false, message: 'The link did not complete. Start it again.' });
  });

  it('is nothing when the page was not reached from a callback', () => {
    expect(linkOutcome(new URLSearchParams(''))).toBeNull();
  });
});

describe('clusterIdentitiesShown', () => {
  it('hides the section where the controller links no cluster', () => {
    expect(clusterIdentitiesShown([], false)).toBe(false);
  });

  it('shows the clusters, or the outcome of a link just made', () => {
    expect(clusterIdentitiesShown([linkCluster()], false)).toBe(true);
    expect(clusterIdentitiesShown([], true)).toBe(true);
  });
});
