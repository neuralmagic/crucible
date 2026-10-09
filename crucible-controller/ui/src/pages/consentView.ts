import type { components } from '../api/schema.d';

export type ConsentDto = components['schemas']['ConsentDto'];

export interface ConsentView {
  title: string;
  /** Whether Allow can succeed: the cluster names who the person would act as. */
  canApprove: boolean;
  /** Why Allow is held back, when it is. */
  blocked: string | null;
}

/// What the consent page asks and whether it can be granted yet. A link cluster nobody has linked
/// cannot be approved: the token would carry no Kubernetes username for the server to check.
export function consentView(consent: ConsentDto): ConsentView {
  const title = `Allow ${consent.client_id}?`;
  const identity = consent.identity;
  if (identity.kube_user) return { title, canApprove: true, blocked: null };
  if (identity.rule.kind === 'link') {
    return {
      title,
      canApprove: false,
      blocked: `Link your username on ${identity.cluster} first.`,
    };
  }
  return {
    title,
    canApprove: false,
    blocked: `Your sign-in carries no ${identity.rule.claim} for ${identity.cluster}; ask an administrator.`,
  };
}

/// The page the cluster's sign-in returns to: this consent page, by id.
export function consentPath(id: string): string {
  return `/consent?id=${encodeURIComponent(id)}`;
}
