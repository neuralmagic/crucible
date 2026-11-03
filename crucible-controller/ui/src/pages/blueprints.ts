import type { components } from '../api/schema';

type UseBlueprintBody = components['schemas']['UseBlueprintBody'];
type PlaceholderReason = components['schemas']['PlaceholderReason'];
type BlueprintSecret = components['schemas']['BlueprintSecretDto'];
type Placeholder = components['schemas']['PlaceholderDto'];

export interface UseForm {
  to: string;
  description: string;
  vaultToken: string;
}

export const EMPTY_USE: UseForm = { to: '', description: '', vaultToken: '' };

/// Pinned to the revision the catalog showed. A blank description takes the blueprint's, and a
/// blank token sends none.
export function blueprintUseBody(form: UseForm, rev: string): UseBlueprintBody {
  const description = form.description.trim();
  const vaultToken = form.vaultToken.trim();
  return {
    id: form.to.trim(),
    expected_rev: rev,
    ...(description.length === 0 ? {} : { description }),
    ...(vaultToken.length === 0 ? {} : { vault_token: vaultToken }),
  };
}

const REASONS: Record<PlaceholderReason, string> = {
  unbound: 'unbound',
  pinned: 'reviewed revisions only',
  personal_secret: 'personal secret',
  other_owner: "another owner's secret",
  not_in_team: 'not in the owning team',
  policy_denied: 'denied by policy',
  unverified: 'needs your Vault token',
};

export function reasonLabel(reason: PlaceholderReason): string {
  return REASONS[reason];
}

/// Where the value goes, `env GH_TOKEN`, or empty when the pack left it to the binding.
export function projectionLabel(placeholder: Placeholder): string {
  const { projection_kind: kind, projection } = placeholder;
  return kind === null || kind === undefined || projection === null || projection === undefined
    ? ''
    : `${kind} ${projection}`;
}

/// What a use would bind for the caller and what it would leave them to bind.
export function secretsSummary(secrets: readonly BlueprintSecret[]): string {
  if (secrets.length === 0) return 'none';
  const carried = secrets.filter((s) => s.carries).length;
  const left = secrets.length - carried;
  return [carried > 0 ? `${carried} carried` : null, left > 0 ? `${left} to bind` : null]
    .filter((part) => part !== null)
    .join(' · ');
}

/// A reference only carries with the caller's own token, so the form asks for one only then.
export function needsVaultToken(secrets: readonly BlueprintSecret[]): boolean {
  return secrets.some((s) => s.reason === 'unverified');
}
