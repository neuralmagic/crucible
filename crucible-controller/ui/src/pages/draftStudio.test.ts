import { describe, expect, it } from 'vitest';
import { NO_AGENT_PICK } from './agentPick';
import type { ParamFieldSpec } from './playbookLaunchForm';
import {
  anchorOf,
  draftLaunchBody,
  EMPTY_STUDIO,
  flaggedFiles,
  isDirty,
  markersFor,
  originLabel,
  originMovedLabel,
  pathsOf,
  saveBody,
  staleBaseOf,
  studioReducer,
  type StudioState,
} from './draftStudio';

const FILES = {
  'workflow.star': 'params = {}\nhello = command()\n',
  'crucible.toml': '[workflow]\n',
  'skills/read/SKILL.md': 'read it\n',
};

function loaded(): StudioState {
  return studioReducer(EMPTY_STUDIO, { kind: 'load', files: FILES, version: 4 });
}

describe('pathsOf', () => {
  it('is every path the pack holds', () => {
    expect(pathsOf(FILES)).toEqual(['crucible.toml', 'skills/read/SKILL.md', 'workflow.star']);
    expect(pathsOf({})).toEqual([]);
  });
});

describe('anchorOf', () => {
  const tabs = pathsOf(FILES);

  it('anchors a pack-relative path onto its tab', () => {
    expect(anchorOf({ file: 'workflow.star', line: 3, col: 7, message: 'x' }, tabs)).toEqual({
      file: 'workflow.star',
      line: 3,
      col: 7,
    });
  });

  it('falls back to the basename when the engine named a longer path', () => {
    expect(anchorOf({ file: 'pack/skills/read/SKILL.md', line: 1, col: null, message: 'x' }, tabs)).toEqual({
      file: 'skills/read/SKILL.md',
      line: 1,
      col: 1,
    });
  });

  it('anchors nothing rather than the wrong file', () => {
    expect(anchorOf({ file: 'elsewhere.star', line: 2, col: 1, message: 'x' }, tabs)).toBeNull();
    expect(anchorOf({ file: null, line: null, col: null, message: 'boom' }, tabs)).toBeNull();
    expect(anchorOf({ file: 'workflow.star', line: null, col: null, message: 'x' }, tabs)).toBeNull();
  });
});

const DIAGNOSTICS = [
  { file: 'workflow.star', line: 9, col: 4, message: 'late' },
  { file: 'skills/read/SKILL.md', line: 1, col: 1, message: 'elsewhere' },
  { file: 'workflow.star', line: 2, col: 8, message: 'early' },
  { file: null, line: null, col: null, message: 'unanchored' },
];

describe('markersFor', () => {
  it('collects one file’s markers at their line and column', () => {
    expect(markersFor('workflow.star', DIAGNOSTICS, pathsOf(FILES))).toEqual([
      { line: 2, col: 8, message: 'early' },
      { line: 9, col: 4, message: 'late' },
    ]);
  });

  it('marks nothing on a file nothing anchors to', () => {
    expect(markersFor('crucible.toml', DIAGNOSTICS, pathsOf(FILES))).toEqual([]);
  });
});

describe('flaggedFiles', () => {
  it('is every file an anchored diagnostic reaches', () => {
    expect([...flaggedFiles(DIAGNOSTICS, pathsOf(FILES))].sort()).toEqual([
      'skills/read/SKILL.md',
      'workflow.star',
    ]);
  });
});

describe('studioReducer', () => {
  it('loads the file map and opens the first file in tree order', () => {
    const state = loaded();
    expect(state.active).toBe('crucible.toml');
    expect(isDirty(state)).toBe(false);
    expect(saveBody(state).files).toEqual(FILES);
    expect(saveBody(state).base_version).toBe(4);
  });

  it('an edit dirties the draft and a save settles it', () => {
    let state = studioReducer(loaded(), { kind: 'edit', file: 'workflow.star', content: 'x\n' });
    expect(isDirty(state)).toBe(true);
    state = studioReducer(state, { kind: 'saveStarted' });
    const inFlight = state.saving;
    expect(inFlight).toBe(1);
    state = studioReducer(state, { kind: 'saveSettled', revision: 1, version: 5 });
    expect(isDirty(state)).toBe(false);
    expect(state.saving).toBeNull();
    expect(saveBody(state).base_version).toBe(5);
  });

  /// The point of tracking revisions: a save that returns after the next keystroke must not claim
  /// the buffer is saved, and must never write the older text back over it.
  it('a save that lands under a newer edit leaves the draft dirty and the buffer alone', () => {
    let state = studioReducer(loaded(), { kind: 'edit', file: 'workflow.star', content: 'first\n' });
    state = studioReducer(state, { kind: 'saveStarted' });
    const posted = state.saving ?? 0;
    state = studioReducer(state, { kind: 'edit', file: 'workflow.star', content: 'second\n' });
    state = studioReducer(state, { kind: 'saveSettled', revision: posted, version: 5 });

    expect(state.files['workflow.star']).toBe('second\n');
    expect(isDirty(state)).toBe(true);
  });

  /// A refused save keeps the buffers and the base it was taken from: the writer still holds their
  /// text, and the next save is against the version they actually read, not the one that overtook
  /// them.
  it('a refused save leaves the buffers and the base alone', () => {
    let state = studioReducer(loaded(), { kind: 'edit', file: 'workflow.star', content: 'mine\n' });
    state = studioReducer(state, { kind: 'saveStarted' });
    state = studioReducer(state, { kind: 'saveFailed' });
    expect(state.saving).toBeNull();
    expect(isDirty(state)).toBe(true);
    expect(state.files['workflow.star']).toBe('mine\n');
    expect(saveBody(state).base_version).toBe(4);
  });

  it('re-typing the same content is not an edit', () => {
    const state = studioReducer(loaded(), {
      kind: 'edit',
      file: 'crucible.toml',
      content: FILES['crucible.toml'],
    });
    expect(isDirty(state)).toBe(false);
  });

  it('selects only files it holds, and adds a new one open and dirty', () => {
    let state = studioReducer(loaded(), { kind: 'select', file: 'nope.md' });
    expect(state.active).toBe('crucible.toml');
    state = studioReducer(state, { kind: 'select', file: 'workflow.star' });
    expect(state.active).toBe('workflow.star');
    state = studioReducer(state, { kind: 'add', file: 'skills/write/SKILL.md' });
    expect(state.active).toBe('skills/write/SKILL.md');
    expect(state.files['skills/write/SKILL.md']).toBe('');
    expect(isDirty(state)).toBe(true);
    const same = studioReducer(state, { kind: 'add', file: 'skills/write/SKILL.md' });
    expect(same).toBe(state);
  });

  it('renames a file and follows it with the open buffer', () => {
    let state = studioReducer(loaded(), { kind: 'select', file: 'skills/read/SKILL.md' });
    state = studioReducer(state, {
      kind: 'rename',
      from: 'skills/read/SKILL.md',
      to: 'skills/read/NOTES.md',
    });
    expect(pathsOf(state.files)).toEqual([
      'crucible.toml',
      'skills/read/NOTES.md',
      'workflow.star',
    ]);
    expect(state.files['skills/read/NOTES.md']).toBe(FILES['skills/read/SKILL.md']);
    expect(state.active).toBe('skills/read/NOTES.md');
    expect(isDirty(state)).toBe(true);
  });

  /// A directory is a prefix, so renaming one rewrites every path under it in one edit.
  it('renames a directory by rewriting the prefix it stands for', () => {
    let state = studioReducer(loaded(), { kind: 'select', file: 'skills/read/SKILL.md' });
    state = studioReducer(state, { kind: 'rename', from: 'skills/', to: 'abilities/' });
    expect(pathsOf(state.files)).toEqual([
      'abilities/read/SKILL.md',
      'crucible.toml',
      'workflow.star',
    ]);
    expect(state.active).toBe('abilities/read/SKILL.md');
  });

  it('refuses a rename onto a path the pack already holds', () => {
    const state = loaded();
    expect(studioReducer(state, { kind: 'rename', from: 'crucible.toml', to: 'workflow.star' })).toBe(
      state
    );
    expect(studioReducer(state, { kind: 'rename', from: 'crucible.toml', to: '' })).toBe(state);
    expect(studioReducer(state, { kind: 'rename', from: 'nope.md', to: 'yes.md' })).toBe(state);
  });

  it('deletes a file and opens what is left', () => {
    const state = studioReducer(loaded(), { kind: 'remove', path: 'crucible.toml' });
    expect(pathsOf(state.files)).toEqual(['skills/read/SKILL.md', 'workflow.star']);
    expect(state.active).toBe('workflow.star');
    expect(isDirty(state)).toBe(true);
  });

  it('deletes a directory whole, and holds still for a path it does not have', () => {
    const state = studioReducer(loaded(), { kind: 'remove', path: 'skills/' });
    expect(pathsOf(state.files)).toEqual(['crucible.toml', 'workflow.star']);
    expect(state.active).toBe('crucible.toml');
    expect(studioReducer(state, { kind: 'remove', path: 'nope.md' })).toBe(state);
  });

  it('leaves nothing open once the last file is deleted', () => {
    let state = loaded();
    for (const path of pathsOf(FILES)) state = studioReducer(state, { kind: 'remove', path });
    expect(state.files).toEqual({});
    expect(state.active).toBe('');
  });
});

describe('staleBaseOf', () => {
  it('reads the version that overtook a save out of the refusal', () => {
    expect(
      staleBaseOf({
        error: 'this save edited version 1',
        base_version: 1,
        current_version: 2,
        saved_by: 'agent:author',
        saved_at: '2026-08-23T10:00:00Z',
      }),
    ).toEqual({
      baseVersion: 1,
      currentVersion: 2,
      savedBy: 'agent:author',
      savedAt: '2026-08-23T10:00:00Z',
    });
  });

  it('an anonymous save still names its version', () => {
    const stale = staleBaseOf({
      base_version: 3,
      current_version: 4,
      saved_by: null,
      saved_at: '2026-08-23T10:00:00Z',
    });
    expect(stale?.savedBy).toBeNull();
    expect(stale?.currentVersion).toBe(4);
  });

  it('any other refusal is not a merge prompt', () => {
    expect(staleBaseOf({ error: 'draft studio retired' })).toBeNull();
    expect(staleBaseOf('boom')).toBeNull();
    expect(staleBaseOf(null)).toBeNull();
    expect(staleBaseOf({ base_version: '1', current_version: 2, saved_at: 'x' })).toBeNull();
  });
});

describe('a draft names what it is based on', () => {
  const PACK = {
    kind: 'playbook',
    playbook: 'survey',
    import_id: null,
    repo: 'neuralmagic/packs',
    path: 'packs/survey',
    rev: '4d5e6f708192a3b4c5d6e7f80912a3b4c5d6e7f8',
    current_rev: '4d5e6f708192a3b4c5d6e7f80912a3b4c5d6e7f8',
    moved: false,
  };

  it('names the pack and the rev it was taken at', () => {
    expect(originLabel(PACK)).toBe('survey@4d5e6f70');
    expect(
      originLabel({
        ...PACK,
        kind: 'import',
        playbook: null,
        import_id: '0192f4a1-2b3c-7d4e-8f90-1a2b3c4d5e6f',
      })
    ).toBe('import 0192f4a1@4d5e6f70');
  });

  it('a draft based on nothing says so', () => {
    expect(originLabel(null)).toBe('—');
    expect(originLabel(undefined)).toBe('—');
    expect(originMovedLabel(null)).toBeNull();
  });

  it('offers the rebase only once the origin actually moved', () => {
    expect(originMovedLabel(PACK)).toBeNull();
    const moved = originMovedLabel({ ...PACK, current_rev: 'aabbccddeeff0011', moved: true });
    expect(moved).toContain('survey');
    expect(moved).toContain('aabbccdd');
    expect(moved).toContain('4d5e6f70');
  });
});

describe('draftLaunchBody', () => {
  const specs: ParamFieldSpec[] = [
    { name: 'topic', required: true, defaultValue: null, pattern: null, description: null },
    { name: 'depth', required: false, defaultValue: null, pattern: null, description: null },
  ];
  const ceilings = { maxCost: 2.5, maxTime: ' 30m ', schemaDigest: 'sha256:bb' };

  it('trims the values, drops a blank optional, and sends a blank required', () => {
    const body = draftLaunchBody(specs, { topic: ' attention ', depth: '  ' }, ceilings, NO_AGENT_PICK);
    expect(body).toEqual({
      params: { topic: 'attention' },
      max_cost: 2.5,
      max_time: '30m',
      schema_digest: 'sha256:bb',
    });
    expect(draftLaunchBody(specs, {}, ceilings, NO_AGENT_PICK).params).toEqual({ topic: '' });
  });

  it('names no provider unless one was picked, and never a model without one', () => {
    const inherited = draftLaunchBody(specs, {}, ceilings, { provider: '', model: 'gpt-5.6-luna' });
    expect('provider' in inherited).toBe(false);
    expect('model' in inherited).toBe(false);

    const pinned = draftLaunchBody(specs, {}, ceilings, { provider: 'plat-openai', model: ' gpt-5.6-luna ' });
    expect(pinned.provider).toBe('plat-openai');
    expect(pinned.model).toBe('gpt-5.6-luna');

    const providerOnly = draftLaunchBody(specs, {}, ceilings, { provider: 'plat-openai', model: '' });
    expect(providerOnly.provider).toBe('plat-openai');
    expect('model' in providerOnly).toBe(false);
  });

  it('omits the digest when the form was rendered against none', () => {
    const body = draftLaunchBody(specs, {}, { ...ceilings, schemaDigest: null }, NO_AGENT_PICK);
    expect('schema_digest' in body && body.schema_digest !== undefined).toBe(false);
  });
});
