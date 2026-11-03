import { describe, expect, it } from 'vitest';
import type { components } from '../api/schema';
import {
  EMPTY_USE,
  blueprintUseBody,
  needsVaultToken,
  projectionLabel,
  reasonLabel,
  secretsSummary,
} from './blueprints';

type BlueprintSecret = components['schemas']['BlueprintSecretDto'];
type PlaceholderReason = components['schemas']['PlaceholderReason'];

const carried: BlueprintSecret = { declared_name: 'gh_token', carries: true, reason: null };
const left = (reason: PlaceholderReason): BlueprintSecret => ({
  declared_name: reason,
  carries: false,
  reason,
});

describe('the use body', () => {
  it('pins the revision and leaves out blanks', () => {
    expect(blueprintUseBody({ ...EMPTY_USE, to: ' copy ' }, 'tree1:aa')).toEqual({
      id: 'copy',
      expected_rev: 'tree1:aa',
    });
    expect(
      blueprintUseBody({ to: 'copy', description: ' mine ', vaultToken: ' hvs.x ' }, 'tree1:aa'),
    ).toEqual({ id: 'copy', expected_rev: 'tree1:aa', description: 'mine', vault_token: 'hvs.x' });
  });
});

describe('the catalog summary', () => {
  it('counts what carries and what is left to bind', () => {
    expect(secretsSummary([])).toBe('none');
    expect(secretsSummary([carried])).toBe('1 carried');
    expect(secretsSummary([left('unbound'), left('pinned')])).toBe('2 to bind');
    expect(secretsSummary([carried, left('not_in_team')])).toBe('1 carried · 1 to bind');
  });

  it('asks for a Vault token only when a reference is waiting on one', () => {
    expect(needsVaultToken([carried, left('personal_secret')])).toBe(false);
    expect(needsVaultToken([carried, left('unverified')])).toBe(true);
  });
});

describe('placeholders', () => {
  it('names every reason', () => {
    const reasons: PlaceholderReason[] = [
      'unbound',
      'pinned',
      'personal_secret',
      'other_owner',
      'not_in_team',
      'policy_denied',
      'unverified',
    ];
    const labels = reasons.map(reasonLabel);
    expect(new Set(labels).size).toBe(reasons.length);
    expect(labels.every((label) => label.length > 0)).toBe(true);
  });

  it('shows where the value goes, or nothing when the pack left it to the binding', () => {
    expect(
      projectionLabel({
        declared_name: 'gh_token',
        projection_kind: 'env',
        projection: 'GH_TOKEN',
        reason: 'unbound',
      }),
    ).toBe('env GH_TOKEN');
    expect(
      projectionLabel({
        declared_name: 'gh_token',
        projection_kind: null,
        projection: null,
        reason: 'unbound',
      }),
    ).toBe('');
  });
});
