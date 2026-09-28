import { describe, expect, it } from 'vitest';
import type { components } from '../api/schema';
import {
  candidatesBody,
  canRegister,
  compileBody,
  graphAwaitsValues,
  matchExistingPlaybook,
  previewOf,
  proposeBody,
  withCandidates,
  withSelection,
  withSource,
} from './playbookImportWizard';

type PackImportDto = components['schemas']['PackImportDto'];
type PlaybookDto = components['schemas']['PlaybookDto'];

const SOURCE = { repo: 'owner/packs', gitRef: 'main' };

const DISPATCH = {
  backend: 'local',
  sandbox_image: null,
  harness: null,
  requires: {},
  prefers: {},
  allow_unverified_image: false,
  resources: { gpus: 0, cpu: null, memory: null, node_selector: {} },
  image: {
    reference: null,
    digest: null,
    capability_digest: null,
    tags: [],
    checked: false,
    catalogued: false,
    verified: false,
    overridden: false,
    unsatisfied: [],
    refusals: [],
    warnings: [],
  },
  dispatchable: true,
  refusal: null,
  local_mode: true,
};

const GRAPH = {
  workflow_type: 'playbook',
  result: 'file',
  nodes: [
    {
      name: 'file',
      kind: 'command' as const,
      required: true,
      needs: 'any' as const,
      join: 'all' as const,
      emits: [],
      emits_files: [],
    },
  ],
  edges: [],
};

function row(overrides: Partial<PackImportDto> = {}): PackImportDto {
  return {
    id: '0199-import',
    owner: 'user:alice',
    repo: 'owner/packs',
    git_ref: 'main',
    path: 'packs/survey',
    rev: 'b'.repeat(40),
    tar_digest: 'sha256:tar',
    params_schema: { type: 'object', properties: {} },
    schema_digest: 'sha256:one',
    graph: GRAPH,
    diagnostics: [],
    dispatch: DISPATCH,
    secrets: { declared: [], warnings: [] },
    exposure: {
      document: null,
      digest: 'sha256:expo',
      lines: ['outputs:', '  draft-pr x1 -> owner/repo'],
    },
    core_rev: 'c'.repeat(40),
    status: 'pending',
    playbook: null,
    draft_id: null,
    proposed_by: 'dana',
    created_at: '2026-08-23T00:00:00Z',
    resolved_by: null,
    resolved_at: null,
    ...overrides,
  };
}

function playbook(overrides: Partial<PlaybookDto> = {}): PlaybookDto {
  return {
    id: 'survey',
    description: 'reads a paper',
    owner: 'user:alice',
    actions: ['read', 'launch'],
    source: { kind: 'git', repo: 'owner/packs', git_ref: 'main', path: 'packs/survey' },
    rev: 'a'.repeat(40),
    tar_digest: 'sha256:tar',
    schema_digest: 'sha256:old',
    core_rev: 'c'.repeat(40),
    dispatch: DISPATCH,
    created_by: 'wren',
    created_at: '2026-08-22T00:00:00Z',
    updated_at: '2026-08-22T00:00:00Z',
    ...overrides,
  };
}

describe('the import source step', () => {
  it('lists candidates with nothing selected', () => {
    const listed = withCandidates(withSource(SOURCE), {
      rev: 'a'.repeat(40),
      candidates: [
        { path: 'packs/survey', workflow_file: 'workflow.star' },
        { path: 'packs/audit', workflow_file: 'workflow.star' },
      ],
    });
    expect(listed.selected).toBeNull();
    expect(withSelection(listed, 'packs/survey').selected).toBe('packs/survey');
  });

  /// A listing from another repo says nothing about this one, so editing the source drops it.
  it('drops the listing when the source changes', () => {
    const listed = withCandidates(withSource(SOURCE), {
      rev: 'a'.repeat(40),
      candidates: [{ path: 'packs/survey', workflow_file: 'workflow.star' }],
    });
    const reSourced = withSource({ repo: 'owner/other', gitRef: 'main' });
    expect(listed.candidates).not.toBeNull();
    expect(reSourced.candidates).toBeNull();
    expect(reSourced.rev).toBeNull();
  });
});

describe('import request bodies', () => {
  it('sends a blank ref as none, so the server picks the default branch', () => {
    expect(candidatesBody({ repo: '  owner/packs  ', gitRef: '   ' })).toEqual({
      repo: 'owner/packs',
      git_ref: null,
    });
    expect(candidatesBody(SOURCE)).toEqual({ repo: 'owner/packs', git_ref: 'main' });
  });

  it('proposes the picked pack directory under the chosen owner and nothing else', () => {
    expect(proposeBody(SOURCE, 'packs/survey', 'team:llm-d')).toEqual({
      repo: 'owner/packs',
      git_ref: 'main',
      path: 'packs/survey',
      owner: 'team:llm-d',
    });
  });

  it('drops blank values rather than compiling against empty strings', () => {
    expect(compileBody({ topic: 'attention', depth: '  ' })).toEqual({
      params: { topic: 'attention' },
    });
  });
});

describe('what the gate renders', () => {
  /// The form is the proposal's; only a re-compile's graph and diagnostics replace the stored
  /// ones, and they come off the same frozen bytes.
  it('prefers a re-compile for the graph and keeps the frozen form', () => {
    const frozen = previewOf(row(), null);
    expect(frozen.schema).toEqual({ type: 'object', properties: {} });
    expect(frozen.graph).toEqual(GRAPH);
    expect(frozen.diagnostics).toEqual([]);

    const recompiled = previewOf(row({ graph: null, diagnostics: ['needs a topic'] }), {
      params_schema: null,
      schema_digest: null,
      graph: GRAPH,
      diagnostics: [],
      dispatch: DISPATCH,
      secrets: { declared: [], warnings: [] },
      exposure: { document: null, digest: null, lines: [] },
    });
    expect(recompiled.graph).toEqual(GRAPH);
    expect(recompiled.diagnostics).toEqual([]);
    expect(recompiled.schemaDigest).toBe('sha256:one');
  });

  it('names the declared secrets before anything is recompiled', () => {
    const declared = {
      declared: [{ name: 'pr_token', kind: 'opaque' as const, projection_kind: 'env' as const, projection: 'PR_TOKEN' }],
      warnings: ['the deploy profile also fills pr_token'],
    };
    expect(previewOf(row({ secrets: declared }), null).secrets).toEqual(declared);
  });

  it('restates them from a re-compile of the same frozen bytes', () => {
    const declared = { declared: [{ name: 'registry', kind: 'registry_authfile' as const }], warnings: [] };
    const recompiled = previewOf(row(), {
      params_schema: null,
      schema_digest: null,
      graph: GRAPH,
      diagnostics: [],
      dispatch: DISPATCH,
      secrets: declared,
      exposure: { document: null, digest: null, lines: [] },
    });
    expect(recompiled.secrets).toEqual(declared);
  });
});

describe('matchExistingPlaybook', () => {
  it('matches on repo and pack directory together', () => {
    const rows = [
      playbook(),
      playbook({
        id: 'audit',
        source: { kind: 'git', repo: 'owner/packs', git_ref: 'main', path: 'packs/audit' },
      }),
      playbook({ id: 'published', source: { kind: 'draft', draft: 'survey', version: 2 } }),
    ];
    expect(matchExistingPlaybook(rows, 'owner/packs', 'packs/survey')?.id).toBe('survey');
    expect(matchExistingPlaybook(rows, ' owner/packs ', 'packs/audit')?.id).toBe('audit');
    expect(matchExistingPlaybook(rows, 'owner/packs', 'packs/other')).toBeNull();
    expect(matchExistingPlaybook(rows, 'someone/else', 'packs/survey')).toBeNull();
    expect(matchExistingPlaybook([], 'owner/packs', 'packs/survey')).toBeNull();
    expect(
      matchExistingPlaybook(rows.slice(2), 'owner/packs', 'packs/survey'),
      'a published draft is never an import target'
    ).toBeNull();
  });
});

describe('what an import permits', () => {
  it('registers a pack whose schema came out, graph or no graph', () => {
    const withGraph = row();
    expect(canRegister(withGraph, previewOf(withGraph, null))).toBe(true);
    expect(graphAwaitsValues(previewOf(withGraph, null))).toBe(false);

    const noGraph = row({
      graph: null,
      diagnostics: ['Error: no value for required parameter "repo"'],
    });
    expect(canRegister(noGraph, previewOf(noGraph, null))).toBe(true);
    expect(graphAwaitsValues(previewOf(noGraph, null))).toBe(true);
  });

  it('registers nothing when the engine refused the source', () => {
    const refused = row({
      params_schema: null,
      schema_digest: null,
      graph: null,
      diagnostics: ['workflow.star:3:5: unknown identifier `dpeth`'],
    });
    expect(canRegister(refused, previewOf(refused, null))).toBe(false);
    expect(graphAwaitsValues(previewOf(refused, null))).toBe(false);
    expect(canRegister(null, null)).toBe(false);
  });

  /// A row that already registered or was discarded is nobody's to act on, however good its form.
  it('registers nothing off a resolved row', () => {
    for (const status of ['registered', 'discarded']) {
      const resolved = row({ status });
      expect(canRegister(resolved, previewOf(resolved, null))).toBe(false);
    }
  });
});
