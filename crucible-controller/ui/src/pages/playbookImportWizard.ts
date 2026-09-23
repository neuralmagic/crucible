import type { components } from '../api/schema';
import type { WorkflowGraphDoc } from './workflowGraphLayout';

type ImportCandidateDto = components['schemas']['ImportCandidateDto'];
type ImportCandidatesBody = components['schemas']['ImportCandidatesBody'];
type ImportCompileDto = components['schemas']['ImportCompileDto'];
type CompileImportBody = components['schemas']['CompileImportBody'];
type PackImportDto = components['schemas']['PackImportDto'];
type PlaybookDto = components['schemas']['PlaybookDto'];
type ProposeImportBody = components['schemas']['ProposeImportBody'];

export interface ImportSource {
  repo: string;
  gitRef: string;
}

export interface ImportState {
  source: ImportSource;
  /// The commit the listing resolved to.
  rev: string | null;
  candidates: readonly ImportCandidateDto[] | null;
  /// The pack directory being proposed, while that POST is in flight.
  selected: string | null;
}

export const INITIAL_IMPORT: ImportState = {
  source: { repo: '', gitRef: '' },
  rev: null,
  candidates: null,
  selected: null,
};

/// Editing the URL or ref invalidates everything downstream: a listing from another repo says
/// nothing about this one.
export function withSource(source: ImportSource): ImportState {
  return { ...INITIAL_IMPORT, source };
}

/// A fresh listing drops whatever the previous one selected.
export function withCandidates(
  state: ImportState,
  listed: { rev: string; candidates: readonly ImportCandidateDto[] }
): ImportState {
  return { ...state, rev: listed.rev, candidates: listed.candidates, selected: null };
}

export function withSelection(state: ImportState, path: string): ImportState {
  return { ...state, selected: path };
}

export function candidatesBody(source: ImportSource): ImportCandidatesBody {
  const gitRef = source.gitRef.trim();
  return { repo: source.repo.trim(), git_ref: gitRef.length > 0 ? gitRef : null };
}

/// The POST that turns a picked candidate into a durable row. The server fetches and compiles;
/// nothing about the pack rides this body.
export function proposeBody(source: ImportSource, path: string, owner: string): ProposeImportBody {
  return { ...candidatesBody(source), path, owner };
}

/// The values a re-compile runs with. Blanks are dropped rather than compiled against empty
/// strings.
export function compileBody(params: Readonly<Record<string, string>>): CompileImportBody {
  const values: Record<string, string> = {};
  for (const [name, value] of Object.entries(params)) {
    if (value.trim().length > 0) values[name] = value;
  }
  return { params: values };
}

/// What the gate renders: the frozen row, with whatever the last re-compile made of the same
/// bytes. Only the graph and the diagnostics move — the form and its digest are the proposal's.
export interface ImportPreview {
  schema: unknown;
  schemaDigest: string | null;
  graph: WorkflowGraphDoc | null;
  diagnostics: readonly string[];
  /// What the pack's agent needs, against what this deployment can dispatch. A re-compile of the
  /// same frozen bytes cannot change it, so the proposal's own reading stands.
  dispatch: PackImportDto['dispatch'];
  /// The credentials the pack declares. Like the dispatch reading, they belong to the frozen bytes
  /// rather than to the values, so a re-compile only ever restates them.
  secrets: PackImportDto['secrets'];
  /// What a run of the proposed pack may write and reach, rendered by the controller. Belongs to
  /// the frozen bytes, so a re-compile restates it.
  exposureLines: readonly string[];
  /// The digest to compare against the registered id this proposal would bump. Null is
  /// absent-legacy.
  exposureDigest: string | null;
}

export function previewOf(
  row: PackImportDto,
  compiled: ImportCompileDto | null | undefined
): ImportPreview {
  const recompiled = compiled ?? null;
  return {
    schema: row.params_schema ?? null,
    schemaDigest: row.schema_digest ?? null,
    graph: recompiled === null ? (row.graph ?? null) : (recompiled.graph ?? null),
    diagnostics: recompiled === null ? row.diagnostics : recompiled.diagnostics,
    dispatch: row.dispatch,
    secrets: recompiled === null ? row.secrets : recompiled.secrets,
    exposureLines: row.exposure.lines,
    exposureDigest: row.exposure.digest ?? null,
  };
}

/// Whether this import can still be registered. Registration stores the params schema, so a
/// proposal that produced one is registrable even when its graph is not drawn — a pack whose
/// required params have no values yet compiles no plan, and that is the launcher's problem, not
/// the importer's. A proposal with no schema is a source the engine refused, and a resolved row is
/// nobody's to act on.
export function canRegister(row: PackImportDto | null, preview: ImportPreview | null): boolean {
  return row !== null && row.status === 'pending' && preview !== null && preview.schema !== null;
}

/// Whether the graph is missing only because the pack was compiled without values.
export function graphAwaitsValues(preview: ImportPreview | null): boolean {
  return preview !== null && preview.schema !== null && preview.graph === null;
}

/// The registered playbook this import would re-pin, if any. Same repo and same pack directory is
/// the same pack: an import that matches one is a pin bump, and its schema diff has to be shown
/// before it is accepted.
export function matchExistingPlaybook(
  playbooks: readonly PlaybookDto[],
  repo: string,
  path: string
): PlaybookDto | null {
  const wanted = repo.trim();
  return (
    playbooks.find(
      (p) => p.source.kind === 'git' && p.source.repo === wanted && p.source.path === path
    ) ?? null
  );
}
