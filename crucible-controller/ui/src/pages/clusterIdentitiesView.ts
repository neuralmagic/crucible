import type { components } from '../api/schema.d';

export type ClusterIdentityDto = components['schemas']['ClusterIdentityDto'];
type IdentityRule = components['schemas']['IdentityRuleDto'];
type IdentitySource = components['schemas']['IdentitySourceDto'];

export type IdentityState = 'resolved' | 'linked' | 'unlinked' | 'override';

/// How a link cluster's row lets the person prove who they are there.
export type LinkMethod = 'oauth' | 'paste';

export interface IdentityView {
  state: IdentityState;
  /** What the rule column reads. */
  rule: string;
  /** The username, or a dash while there is none. */
  username: string;
  /** The status chip. */
  headline: string;
  /** The line under the row: where the name came from, or what to do. */
  detail: string;
  /** `null` on a rule cluster, or while an administrator's override holds the name. */
  link: { method: LinkMethod; label: 'Link' | 'Relink' } | null;
  unlinkable: boolean;
}

export function ruleLabel(rule: IdentityRule): string {
  if (rule.kind === 'link') {
    return rule.oauth ? 'Linked, OpenShift sign-in' : 'Linked, pasted token';
  }
  const claim = rule.claim === 'login' ? 'SSO login' : 'SSO email';
  return rule.prefix ? `${claim}, prefixed ${rule.prefix}` : claim;
}

const OVERRIDE = 'Set by a platform administrator. Only another administrator can change it.';

const PROVEN: Record<IdentitySource, string> = {
  rule: 'Read off your SSO sign-in',
  'openshift-oauth': 'Proven by signing in to the cluster',
  'token-review': "Proven by the cluster's TokenReview of a token you pasted",
  override: 'Set by a platform administrator',
};

/// What one cluster's row says and offers. A rule cluster is never editable here: its name is
/// whatever the SSO login says, so the fix for a wrong one is at the identity provider.
export function identityView(identity: ClusterIdentityDto): IdentityView {
  const rule = ruleLabel(identity.rule);
  const username = identity.kube_user ?? '—';
  if (identity.source === 'override') {
    return {
      state: 'override',
      rule,
      username,
      headline: 'OVERRIDDEN',
      detail: OVERRIDE,
      link: null,
      unlinkable: false,
    };
  }
  if (identity.rule.kind === 'claim') {
    return {
      state: 'resolved',
      rule,
      username,
      headline: 'FROM SSO',
      detail: identity.kube_user
        ? `${PROVEN.rule}.`
        : 'Your SSO sign-in carries no value for this claim.',
      link: null,
      unlinkable: false,
    };
  }
  const method: LinkMethod = identity.rule.oauth ? 'oauth' : 'paste';
  if (!identity.kube_user) {
    return {
      state: 'unlinked',
      rule,
      username,
      headline: 'NOT LINKED',
      detail:
        method === 'oauth'
          ? 'Sign in to the cluster once and the controller records who it says you are.'
          : 'Paste `oc whoami -t` or an OIDC ID token for this cluster. It is checked and dropped, never stored.',
      link: { method, label: 'Link' },
      unlinkable: false,
    };
  }
  const how = identity.source ? PROVEN[identity.source] : 'Linked';
  const when = identity.linked_at ? ` on ${identity.linked_at}.` : '.';
  return {
    state: 'linked',
    rule,
    username,
    headline: 'LINKED',
    detail: `${how}${when}`,
    link: { method, label: 'Relink' },
    unlinkable: true,
  };
}

/// Clusters in a stable order: the ones waiting on the person first, then by name.
export function sortIdentities(identities: readonly ClusterIdentityDto[]): ClusterIdentityDto[] {
  const waiting = (i: ClusterIdentityDto) => (i.rule.kind === 'link' && !i.kube_user ? 0 : 1);
  return [...identities].sort(
    (a, b) => waiting(a) - waiting(b) || a.cluster.localeCompare(b.cluster),
  );
}

/// The callback lands the browser back on Settings with an outcome code, never free text, so a
/// crafted link cannot put words on this page.
const LINK_ERRORS: Record<string, string> = {
  'no-flow': 'That sign-in answered no link this browser started. Start the link again.',
  expired: 'The link took longer than ten minutes. Start it again.',
  'state-mismatch': 'The sign-in did not match the link this browser started. Start it again.',
  'wrong-cluster': 'The sign-in came back for a different cluster than the one you linked.',
  'wrong-person': 'The link was started by someone else signed in to this browser.',
  refused: 'The cluster refused the sign-in.',
  exchange: 'The cluster did not accept the sign-in code. Retry, or tell an administrator.',
  review: 'The cluster did not confirm who the token belongs to.',
  unavailable: 'The cluster could not be reached. Retry.',
};

export type LinkOutcome = { ok: true; cluster: string } | { ok: false; message: string };

export function linkOutcome(search: URLSearchParams): LinkOutcome | null {
  const linked = search.get('linked');
  if (linked) return { ok: true, cluster: linked };
  const error = search.get('link_error');
  if (error === null) return null;
  return {
    ok: false,
    message: LINK_ERRORS[error] ?? 'The link did not complete. Start it again.',
  };
}

/** The section shows when the controller links any cluster, or to report a link just made. */
export function clusterIdentitiesShown(identities: readonly unknown[], hasOutcome: boolean): boolean {
  return identities.length > 0 || hasOutcome;
}
