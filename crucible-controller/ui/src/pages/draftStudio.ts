import type { components } from '../api/schema';
import { fileTree, firstFile } from '../editor/fileTree';
import { agentPickFields, type AgentPick } from './agentPick';
import { trimmedParams, type ParamFieldSpec } from './playbookLaunchForm';

type DraftDiagnostic = components['schemas']['Diagnostic'];
type LaunchDraftBody = components['schemas']['LaunchDraftBody'];

/// The pack's paths in the order the file tree lists them. A pack is read manifest-first, so it is
/// edited in that order, and the studio opens on whatever comes first.
export function pathsOf(files: Readonly<Record<string, string>>): string[] {
  return Object.keys(files).sort();
}

/// Where a diagnostic belongs in the editor. The engine names the source file it was handed, which
/// is the pack-relative path; a name that matches nothing falls back to the basename, and one that
/// still matches nothing anchors nowhere rather than pointing at the wrong file.
export function anchorOf(
  diagnostic: DraftDiagnostic,
  paths: readonly string[]
): { file: string; line: number; col: number } | null {
  const named = diagnostic.file;
  if (named === null || named === undefined || diagnostic.line === null || diagnostic.line === undefined) {
    return null;
  }
  const exact = paths.find((path) => path === named);
  const base = named.split('/').pop() ?? named;
  const file = exact ?? paths.find((path) => (path.split('/').pop() ?? path) === base);
  if (file === undefined) return null;
  return { file, line: diagnostic.line, col: diagnostic.col ?? 1 };
}

/// The diagnostics anchored to one file, as Monaco markers, in position order.
export function markersFor(
  file: string,
  diagnostics: readonly DraftDiagnostic[],
  paths: readonly string[]
): { line: number; col: number; message: string }[] {
  return diagnostics
    .flatMap((diagnostic) => {
      const anchor = anchorOf(diagnostic, paths);
      if (anchor === null || anchor.file !== file) return [];
      return [{ line: anchor.line, col: anchor.col, message: diagnostic.message }];
    })
    .sort((a, b) => a.line - b.line || a.col - b.col);
}

/// Every file at least one diagnostic anchors to, so the tree can mark them.
export function flaggedFiles(
  diagnostics: readonly DraftDiagnostic[],
  paths: readonly string[]
): Set<string> {
  return new Set(
    diagnostics.flatMap((diagnostic) => {
      const anchor = anchorOf(diagnostic, paths);
      return anchor === null ? [] : [anchor.file];
    })
  );
}

/// The editor's buffers and what is saved. `revision` counts edits; `savedRevision` is the
/// revision the last accepted save was taken at, so a save that lands while a newer edit is
/// pending leaves the draft dirty instead of declaring it clean. `base` is the stored version the
/// buffers were taken from, which every save carries so the other editor's work is not overwritten.
export interface StudioState {
  files: Record<string, string>;
  active: string;
  revision: number;
  savedRevision: number;
  /// The revision a save is in flight for; null when none is.
  saving: number | null;
  base: number;
}

export type StudioAction =
  | { kind: 'load'; files: Record<string, string>; version: number }
  | { kind: 'edit'; file: string; content: string }
  | { kind: 'select'; file: string }
  | { kind: 'add'; file: string }
  | { kind: 'rename'; from: string; to: string }
  | { kind: 'remove'; path: string }
  | { kind: 'saveStarted' }
  | { kind: 'saveSettled'; revision: number; version: number }
  | { kind: 'saveFailed' };

export const EMPTY_STUDIO: StudioState = {
  files: {},
  active: '',
  revision: 0,
  savedRevision: 0,
  saving: null,
  base: 0,
};

/// A directory id ends in a slash, so a prefix rewrite covers exactly the files under it and a
/// file covers only itself.
function covers(path: string, id: string): boolean {
  return id.endsWith('/') ? path.startsWith(id) : path === id;
}

function moved(path: string, from: string, to: string): string {
  return from.endsWith('/') ? `${to}${path.slice(from.length)}` : to;
}

export function studioReducer(state: StudioState, action: StudioAction): StudioState {
  switch (action.kind) {
    case 'load': {
      return {
        files: { ...action.files },
        active: firstFile(fileTree(pathsOf(action.files))) ?? '',
        revision: 0,
        savedRevision: 0,
        saving: null,
        base: action.version,
      };
    }
    case 'edit': {
      if (state.files[action.file] === action.content) return state;
      return {
        ...state,
        files: { ...state.files, [action.file]: action.content },
        revision: state.revision + 1,
      };
    }
    case 'select':
      return state.files[action.file] === undefined ? state : { ...state, active: action.file };
    case 'add': {
      if (action.file.length === 0 || state.files[action.file] !== undefined) return state;
      return {
        ...state,
        files: { ...state.files, [action.file]: '' },
        active: action.file,
        revision: state.revision + 1,
      };
    }
    case 'rename': {
      const { from, to } = action;
      if (to.length === 0 || to === from) return state;
      const taken = Object.keys(state.files).filter((path) => covers(path, from));
      if (taken.length === 0) return state;
      const files: Record<string, string> = {};
      for (const [path, content] of Object.entries(state.files)) {
        const next = covers(path, from) ? moved(path, from, to) : path;
        if (next !== path && state.files[next] !== undefined) return state;
        files[next] = content;
      }
      return {
        ...state,
        files,
        active: covers(state.active, from) ? moved(state.active, from, to) : state.active,
        revision: state.revision + 1,
      };
    }
    case 'remove': {
      const files: Record<string, string> = {};
      for (const [path, content] of Object.entries(state.files)) {
        if (!covers(path, action.path)) files[path] = content;
      }
      if (Object.keys(files).length === Object.keys(state.files).length) return state;
      const active = covers(state.active, action.path)
        ? (firstFile(fileTree(pathsOf(files))) ?? '')
        : state.active;
      return { ...state, files, active, revision: state.revision + 1 };
    }
    case 'saveStarted':
      return { ...state, saving: state.revision };
    case 'saveSettled':
      // The buffers are never rewritten by a save: what came back describes the revision that was
      // posted, and anything typed since is still the newer text.
      return {
        ...state,
        saving: null,
        savedRevision: Math.max(state.savedRevision, action.revision),
        base: Math.max(state.base, action.version),
      };
    case 'saveFailed':
      return { ...state, saving: null };
  }
}

export function isDirty(state: StudioState): boolean {
  return state.revision !== state.savedRevision;
}

/// The body of a draft test-fire: the trimmed form values (a blank optional stays out, a blank
/// required goes so the endpoint answers for it), the ceilings, the digest the form was rendered
/// against, and the provider pin when one was picked.
export function draftLaunchBody(
  specs: readonly ParamFieldSpec[],
  values: Readonly<Record<string, string>>,
  ceilings: { maxCost: number; maxTime: string; schemaDigest: string | null },
  agent: AgentPick
): LaunchDraftBody {
  const params = trimmedParams(specs, values);
  return {
    params,
    max_cost: ceilings.maxCost,
    max_time: ceilings.maxTime.trim(),
    schema_digest: ceilings.schemaDigest ?? undefined,
    ...agentPickFields(agent),
  };
}

/// The POST body one save sends: the whole tree, since a save is the pack, and the version it was
/// taken from, since a save that no longer follows the latest one is refused rather than applied.
export function saveBody(state: StudioState): {
  files: Record<string, string>;
  base_version: number;
} {
  return { files: { ...state.files }, base_version: state.base };
}

/// Somebody else's save that overtook this one. The controller answers a stale base with 409 and
/// this body; anything else is an ordinary error and renders as its message.
export interface StaleBase {
  baseVersion: number;
  currentVersion: number;
  savedBy: string | null;
  savedAt: string;
}

export function staleBaseOf(err: unknown): StaleBase | null {
  if (typeof err !== 'object' || err === null) return null;
  if (!('base_version' in err) || !('current_version' in err) || !('saved_at' in err)) return null;
  const base = err.base_version;
  const current = err.current_version;
  const savedAt = err.saved_at;
  if (typeof base !== 'number' || typeof current !== 'number' || typeof savedAt !== 'string') {
    return null;
  }
  const savedBy = 'saved_by' in err ? err.saved_by : null;
  return {
    baseVersion: base,
    currentVersion: current,
    savedBy: typeof savedBy === 'string' ? savedBy : null,
    savedAt,
  };
}

type DraftOrigin = components['schemas']['DraftOriginDto'];

/// What a draft is based on, in one line: the pack and the rev it was taken at. A rev is shown
/// short, the way a commit is read.
export function originLabel(origin: DraftOrigin | null | undefined): string {
  if (origin === null || origin === undefined) return '—';
  const name =
    origin.kind === 'playbook'
      ? (origin.playbook ?? 'a pack')
      : `import ${shortRev(origin.import_id)}`;
  const rev = shortRev(origin.rev);
  return rev === '' ? name : `${name}@${rev}`;
}

/// The rebase prompt: which rev the origin serves now, against the one the draft was taken at.
export function originMovedLabel(origin: DraftOrigin | null | undefined): string | null {
  if (origin === null || origin === undefined || !origin.moved) return null;
  const name = origin.kind === 'playbook' ? (origin.playbook ?? 'the origin pack') : 'the origin';
  return `${name} re-pinned to ${shortRev(origin.current_rev)} since this draft was taken at ${shortRev(origin.rev)}.`;
}

function shortRev(rev: string | null | undefined): string {
  if (rev === null || rev === undefined) return '';
  return rev.length > 8 ? rev.slice(0, 8) : rev;
}
