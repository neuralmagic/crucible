import type { components } from '../api/schema.d';
import { clientLabel } from './mcpSignInsView';

export type ConsentDto = components['schemas']['ConsentDto'];

export interface ConsentView {
  title: string;
  /** Who asks: the client's name, with the host that published it for a metadata document client. */
  client: string;
  /** For a metadata document client, a warning that only its host vouches for its name. */
  publisher: string | null;
  /** Whether Allow can succeed: the cluster names who the person would act as. */
  canApprove: boolean;
  /** Why Allow is held back, when it is. */
  blocked: string | null;
}

/// What the consent page asks and whether it can be granted yet. A link cluster nobody has linked
/// cannot be approved: the token would carry no Kubernetes username for the server to check.
export function consentView(consent: ConsentDto): ConsentView {
  const client = clientLabel(consent);
  const title = `Allow ${client}?`;
  const publisher = consent.client_host
    ? `${consent.client_host} published this client and its name; this controller does not register it.`
    : null;
  const shown = { title, client, publisher };
  const identity = consent.identity;
  if (identity.kube_user) return { ...shown, canApprove: true, blocked: null };
  if (identity.rule.kind === 'link') {
    return {
      ...shown,
      canApprove: false,
      blocked: `Link your username on ${identity.cluster} first.`,
    };
  }
  return {
    ...shown,
    canApprove: false,
    blocked: `Your sign-in carries no ${identity.rule.claim} for ${identity.cluster}; ask an administrator.`,
  };
}

/// The page the cluster's sign-in returns to: this consent page, by id.
export function consentPath(id: string): string {
  return `/consent?id=${encodeURIComponent(id)}`;
}
