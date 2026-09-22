import { describe, expect, it } from 'vitest';
import {
  agentVisibleWarning,
  credentialErrors,
  credentialsValue,
  bindBody,
  bindErrors,
  canBind,
  canRegister,
  declaredLabel,
  deleteBlocker,
  emptyBindForm,
  emptyRegisterForm,
  groupSecrets,
  hasDeclaredSecrets,
  nameError,
  originLabel,
  projectionKindOptions,
  registerBody,
  registerErrors,
  visibilityOptions,
  withKind,
  withOwners,
  withVisibility,
  type BindForm,
  type RegisterForm,
  type SecretBindingDto,
  type SecretDto,
} from './secretsView';

function form(over: Partial<RegisterForm> = {}): RegisterForm {
  return { ...emptyRegisterForm('user:will'), name: 'pr_token', value: 'hunter2', ...over };
}

function secret(over: Partial<SecretDto> = {}): SecretDto {
  return {
    id: 's1',
    name: 'pr_token',
    owner: 'user:will',
    kind: 'opaque',
    visibility: 'broker_only',
    consumer: 'run',
    mode: 'managed',
    vault_path: 'user:will/pr_token',
    current_version: 3,
    created_by: 'will',
    created_at: '2026-08-24T10:00:00Z',
    updated_at: '2026-08-24T10:00:00Z',
    ...over,
  };
}

function binding(over: Partial<SecretBindingDto> = {}): SecretBindingDto {
  return {
    id: 'b1',
    secret_id: 's1',
    scope_kind: 'repo',
    scope_id: 'neuralmagic/crucible',
    projection_kind: 'env',
    projection: 'AUTORESEARCH_PR_TOKEN',
    declared_name: 'pr_token',
    pack_rev: null,
    schema_digest: null,
    created_by: 'will',
    created_at: '2026-08-24T10:00:00Z',
    ...over,
  };
}

describe('withOwners', () => {
  const options = [
    { value: 'user:will', label: 'user:will' },
    { value: 'group:/groups/team-x', label: 'group:/groups/team-x' },
  ];

  it('leaves an owner the caller actually holds alone', () => {
    const held = form({ owner: 'group:/groups/team-x' });
    expect(withOwners(held, options)).toBe(held);
  });

  /// The options arrive with the session, one render after the form is first built.
  it('falls back to the first option when the form holds nothing offered', () => {
    expect(withOwners(form({ owner: '' }), options).owner).toBe('user:will');
    expect(withOwners(form({ owner: 'group:/groups/gone' }), options).owner).toBe('user:will');
  });

  it('leaves the form unregisterable when nobody is signed in', () => {
    const anonymous = withOwners(form({ owner: 'user:will' }), []);
    expect(anonymous.owner).toBe('');
    expect(canRegister(anonymous)).toBe(false);
  });
});

describe('nameError', () => {
  it('accepts the character set the registry parses', () => {
    expect(nameError(' PR_Token ')).toBeNull();
    expect(nameError('quay.push-1')).toBeNull();
  });

  it('names what is wrong', () => {
    expect(nameError('')).toBe('a secret name cannot be empty');
    expect(nameError('a'.repeat(97))).toContain('at most 96');
    expect(nameError('pr token')).toContain('outside [a-z0-9._-]');
    expect(nameError('a/b')).toContain('outside [a-z0-9._-]');
  });
});

describe('visibility and projection menus', () => {
  it('offers agent_visible only for an opaque secret', () => {
    expect(visibilityOptions('opaque').map((o) => o.value)).toEqual([
      'broker_only',
      'agent_visible',
    ]);
    for (const kind of ['file', 'registry_authfile', 'kubeconfig'] as const) {
      expect(visibilityOptions(kind).map((o) => o.value)).toEqual(['broker_only']);
    }
  });

  it('offers env for the key-shaped kinds and file only for the rest', () => {
    expect(projectionKindOptions('opaque').map((o) => o.value)).toEqual(['env', 'file']);
    expect(projectionKindOptions('inference_api_key').map((o) => o.value)).toEqual(['env', 'file']);
    expect(projectionKindOptions('registry_authfile').map((o) => o.value)).toEqual(['file']);
    expect(visibilityOptions('inference_api_key').map((o) => o.value)).toEqual(['broker_only']);
  });
});

describe('the agent_visible warning', () => {
  it('is silent while the value stays broker-side', () => {
    expect(agentVisibleWarning(form())).toBeNull();
  });

  it('says the value can end up in a PR', () => {
    const warned = withVisibility(form(), 'agent_visible');
    expect(agentVisibleWarning(warned)).toContain('pull request');
  });

  it('blocks the save until it is acknowledged', () => {
    const warned = withVisibility(form(), 'agent_visible');
    expect(registerErrors(warned).size).toBe(0);
    expect(canRegister(warned)).toBe(false);
    expect(canRegister({ ...warned, acknowledged: true })).toBe(true);
  });

  it('is re-asked whenever the visibility is chosen again', () => {
    const acknowledged = { ...withVisibility(form(), 'agent_visible'), acknowledged: true };
    expect(canRegister(withVisibility(acknowledged, 'agent_visible'))).toBe(false);
  });

  it('goes away with the kind that could take it, dropping the acknowledgement', () => {
    const acknowledged = { ...withVisibility(form(), 'agent_visible'), acknowledged: true };
    const asFile = withKind(acknowledged, 'file');
    expect(asFile.visibility).toBe('broker_only');
    expect(asFile.acknowledged).toBe(false);
    expect(agentVisibleWarning(asFile)).toBeNull();
    expect(canRegister(asFile)).toBe(true);
  });

  it('survives a kind change that can still take it', () => {
    const acknowledged = { ...withVisibility(form(), 'agent_visible'), acknowledged: true };
    expect(withKind(acknowledged, 'opaque').acknowledged).toBe(true);
  });
});

describe('registerErrors', () => {
  it('is clean for a value registration', () => {
    expect([...registerErrors(form()).keys()]).toEqual([]);
  });

  it('refuses an empty value', () => {
    expect(registerErrors(form({ value: '' })).get('value')).toBe(
      'a registered value cannot be empty',
    );
  });

  it('refuses a reference that is not a vault pointer, and one with no token', () => {
    const asReference = form({ mode: 'reference', value: '', reference: 'https://vault/x' });
    expect(registerErrors(asReference).get('reference')).toBe('a reference starts with vault://');
    expect(registerErrors(asReference).get('vaultToken')).toContain('your own Vault token');
    const good = { ...asReference, reference: 'vault://kv/team/x#token', vaultToken: 'hvs.aaa' };
    expect(registerErrors(good).size).toBe(0);
    expect(canRegister(good)).toBe(true);
  });

  it('refuses agent_visible on a kind that cannot take it', () => {
    const bad = { ...form(), kind: 'file' as const, visibility: 'agent_visible' as const };
    expect(registerErrors(bad).get('visibility')).toContain('broker-side only');
    expect(canRegister(bad)).toBe(false);
  });

  it('refuses an anonymous owner', () => {
    expect(registerErrors(form({ owner: '' })).get('owner')).toBe('sign in to own a secret');
  });

  it('needs neither a value nor a token to mint, but does need an opaque kind', () => {
    const minted = form({ mode: 'mint', value: '', vaultToken: '' });
    expect(registerErrors(minted).size).toBe(0);
    expect(canRegister(minted)).toBe(true);
    const asFile = { ...minted, kind: 'registry_authfile' as const };
    expect(registerErrors(asFile).get('kind')).toContain('cannot be minted');
    expect(canRegister(asFile)).toBe(false);
  });
});

describe('registerBody', () => {
  it('normalizes the name and sends the value alone', () => {
    expect(registerBody(form({ name: ' PR_Token ' }))).toEqual({
      name: 'pr_token',
      owner: 'user:will',
      kind: 'opaque',
      visibility: 'broker_only',
      value: 'hunter2',
    });
  });

  it('sends the pointer and the token in reference mode, and no value', () => {
    const body = registerBody(
      form({
        mode: 'reference',
        value: 'hunter2',
        reference: ' vault://kv/team/x#token ',
        vaultToken: ' hvs.aaa ',
      }),
    );
    expect(body).toEqual({
      name: 'pr_token',
      owner: 'user:will',
      kind: 'opaque',
      visibility: 'broker_only',
      reference: 'vault://kv/team/x#token',
      vault_token: 'hvs.aaa',
    });
    expect('value' in body).toBe(false);
  });

  it('sends the minter alone in mint mode, and neither a value nor a token', () => {
    const body = registerBody(
      form({ mode: 'mint', value: 'hunter2', vaultToken: 'hvs.aaa', minter: 'github-app' }),
    );
    expect(body).toEqual({
      name: 'pr_token',
      owner: 'user:will',
      kind: 'opaque',
      visibility: 'broker_only',
      mint: 'github-app',
    });
    expect('value' in body).toBe(false);
    expect('vault_token' in body).toBe(false);
  });
});

describe('the bind form', () => {
  function bound(over: Partial<BindForm> = {}): BindForm {
    return {
      ...emptyBindForm('opaque'),
      scopeId: 'neuralmagic/crucible',
      projection: 'AUTORESEARCH_PR_TOKEN',
      ...over,
    };
  }

  it("defaults its projection kind to what the secret's kind allows", () => {
    expect(emptyBindForm('opaque').projectionKind).toBe('env');
    expect(emptyBindForm('registry_authfile').projectionKind).toBe('file');
  });

  it('needs a scope id and a projection', () => {
    const errors = bindErrors(bound({ scopeId: ' ', projection: '' }), secret());
    expect(errors.get('scopeId')).toBe('a binding needs a scope id');
    expect(errors.get('projection')).toBe('a binding needs a projection');
  });

  it('refuses an env projection for a file-shaped kind', () => {
    const authfile = secret({ kind: 'registry_authfile' });
    expect(bindErrors(bound({ projectionKind: 'env' }), authfile).get('projectionKind')).toContain(
      'projected as a file',
    );
    expect(canBind(bound({ projectionKind: 'file', projection: '/etc/quay.json' }), authfile)).toBe(
      true,
    );
  });

  it("needs the binder's own Vault token for a reference", () => {
    const reference = secret({ mode: 'reference' });
    expect(bindErrors(bound(), reference).get('vaultToken')).toContain('your own Vault token');
    expect(canBind(bound({ vaultToken: 'hvs.aaa' }), reference)).toBe(true);
  });

  it("validates a declared name that overrides the secret's own", () => {
    expect(bindErrors(bound({ declaredName: 'not a name' }), secret()).get('declaredName')).toContain(
      'outside [a-z0-9._-]',
    );
  });

  it("leaves the declared name off when it is the secret's own, and normalizes it when given", () => {
    expect(bindBody(bound(), secret())).toEqual({
      scope_kind: 'repo',
      scope_id: 'neuralmagic/crucible',
      projection_kind: 'env',
      projection: 'AUTORESEARCH_PR_TOKEN',
    });
    expect(bindBody(bound({ declaredName: ' PR_Token ' }), secret()).declared_name).toBe('pr_token');
  });

  it('carries the Vault token only for a reference', () => {
    expect(bindBody(bound({ vaultToken: 'hvs.aaa' }), secret()).vault_token).toBeUndefined();
    expect(bindBody(bound({ vaultToken: 'hvs.aaa' }), secret({ mode: 'reference' })).vault_token).toBe(
      'hvs.aaa',
    );
  });
});

describe('groupSecrets', () => {
  it("splits the caller's own from the ones their teams own", () => {
    const mine = secret({ id: 'a', owner: 'user:will' });
    const theirs = secret({ id: 'b', owner: 'group:/groups/team-x' });
    expect(groupSecrets([mine, theirs], 'Will')).toEqual({ own: [mine], team: [theirs] });
  });

  it('calls everything a team secret when nobody is signed in', () => {
    const row = secret();
    expect(groupSecrets([row], null)).toEqual({ own: [], team: [row] });
  });
});

describe('originLabel', () => {
  it('shows a managed path at its version and a reference as its pointer', () => {
    expect(originLabel(secret())).toBe('user:will/pr_token @ v3');
    expect(originLabel(secret({ current_version: null }))).toBe('user:will/pr_token');
    expect(
      originLabel(secret({ mode: 'reference', vault_path: 'vault://kv/team/x#token', current_version: null })),
    ).toBe('vault://kv/team/x#token');
  });

  it('says a minted secret is issued rather than stored', () => {
    expect(
      originLabel(secret({ mode: 'minted', vault_path: 'mint://github-app', current_version: null })),
    ).toBe('mint://github-app (issued at every dispatch)');
  });

  it('never carries anything but a location', () => {
    expect(originLabel(secret())).not.toContain('hunter2');
  });
});

describe('deleteBlocker', () => {
  it('is clear when nothing is bound', () => {
    expect(deleteBlocker([])).toBeNull();
  });

  it('names every scope holding the secret', () => {
    const blocker = deleteBlocker([binding(), binding({ id: 'b2', scope_kind: 'playbook', scope_id: 'sweep' })]);
    expect(blocker).toBe('still bound to repo neuralmagic/crucible, playbook sweep; unbind it first');
  });
});

describe('declared secrets', () => {
  it('names the declared secret, its kind, and the projection the pack expects', () => {
    expect(
      declaredLabel({ name: 'pr_token', kind: 'opaque', projection_kind: 'env', projection: 'PR_TOKEN' }),
    ).toBe('pr_token (opaque) → env PR_TOKEN');
  });

  it('names a declaration that left its projection to the binding', () => {
    expect(declaredLabel({ name: 'pr_token', kind: 'opaque' })).toBe('pr_token (opaque)');
  });

  it('has a section to draw only when the pack declares something', () => {
    expect(hasDeclaredSecrets(null)).toBe(false);
    expect(hasDeclaredSecrets({ declared: [], warnings: [] })).toBe(false);
    expect(
      hasDeclaredSecrets({ declared: [{ name: 'pr_token', kind: 'opaque' }], warnings: [] }),
    ).toBe(true);
  });
});

describe('inference credentials', () => {
  it('assembles the rows into one credentials map and drops untouched rows', () => {
    const value = credentialsValue([
      { name: ' OPENAI_API_KEY ', value: 'sk-1' },
      { name: 'OPENAI_ORG_ID', value: 'org-1' },
      { name: '', value: '' },
    ]);
    expect(JSON.parse(value)).toEqual({ OPENAI_API_KEY: 'sk-1', OPENAI_ORG_ID: 'org-1' });
  });

  it('refuses what the server would refuse, row by row', () => {
    expect(credentialErrors([{ name: 'OPENAI_API_KEY', value: 'sk' }]).size).toBe(0);
    expect(credentialErrors([{ name: 'openai_api_key', value: 'sk' }]).get(0)).toContain('environment variable name');
    expect(credentialErrors([{ name: 'OPENAI_API_KEY', value: '' }]).get(0)).toContain('no value');
    expect(
      credentialErrors([
        { name: 'OPENAI_API_KEY', value: 'a' },
        { name: 'OPENAI_API_KEY', value: 'b' },
      ]).get(1)
    ).toContain('twice');
    expect(credentialErrors([{ name: '', value: '' }]).get(0)).toContain('at least one');
  });

  it('registers an inference key as its map, and a plain kind as its value', () => {
    const form = { ...emptyRegisterForm('user:alice'), name: 'openai-key' };
    const plain = registerBody({ ...form, value: 'sk-plain' });
    expect(plain.value).toBe('sk-plain');
    const inference = registerBody({
      ...form,
      kind: 'inference_api_key',
      credentials: [{ name: 'OPENAI_API_KEY', value: 'sk-1' }],
    });
    expect(inference.kind).toBe('inference_api_key');
    expect(JSON.parse(inference.value ?? '')).toEqual({ OPENAI_API_KEY: 'sk-1' });
    const errors = registerErrors({ ...form, kind: 'inference_api_key', credentials: [{ name: 'bad name', value: 'x' }] });
    expect(errors.get('value')).toContain('environment variable name');
  });
});
