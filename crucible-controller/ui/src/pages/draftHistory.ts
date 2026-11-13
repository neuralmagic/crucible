import type { components } from '../api/schema';

export type HistoryEntry = components['schemas']['DraftHistoryEntryDto'];
export type FileChange = components['schemas']['FileChange'];
export type FileDiff = components['schemas']['DraftFileDiffDto'];
type PlaybookRun = components['schemas']['PlaybookRunDto'];

/// Two saves to compare. A null `from` is the empty pack the first save is diffed against.
export interface Range {
  from: number | null;
  to: number;
}

function versionOf(raw: string | null, history: readonly HistoryEntry[]): number | null {
  if (raw === null || !/^\d+$/.test(raw)) return null;
  const version = Number(raw);
  return history.some((entry) => entry.version === version) ? version : null;
}

function previousOf(history: readonly HistoryEntry[], version: number): number | null {
  return history.find((entry) => entry.version === version)?.previous_version ?? null;
}

/// What one save changed: that save against the one before it.
export function singleSave(history: readonly HistoryEntry[], version: number): Range {
  return { from: previousOf(history, version), to: version };
}

/// The range a URL names, held to versions the draft has. An absent or unknown `to` is the newest
/// save, and an absent or unknown `from` is the save before `to`. Null while there are no saves.
export function rangeOf(params: URLSearchParams, history: readonly HistoryEntry[]): Range | null {
  const newest = history[0];
  if (newest === undefined) return null;
  const to = versionOf(params.get('to'), history) ?? newest.version;
  const from = versionOf(params.get('from'), history);
  return from === null ? singleSave(history, to) : { from, to };
}

export function rangeSearch(range: Range): string {
  const params = new URLSearchParams();
  if (range.from !== null) params.set('from', String(range.from));
  params.set('to', String(range.to));
  return `?${params.toString()}`;
}

export function rangeLabel(range: Range): string {
  return `${range.from === null ? 'empty' : `v${range.from}`} → v${range.to}`;
}

/// The saves a forward range covers: every version after `from` up to and including `to`, oldest
/// first. Empty for a range that runs backward or compares a save with itself.
export function savesIn(history: readonly HistoryEntry[], range: Range): number[] {
  const from = range.from ?? Number.NEGATIVE_INFINITY;
  return history
    .map((entry) => entry.version)
    .filter((version) => version > from && version <= range.to)
    .sort((a, b) => a - b);
}

/// Both halves of a range that covers more than one save, split at its middle save. Null when
/// there is nothing left to halve.
export function halvesOf(
  history: readonly HistoryEntry[],
  range: Range,
): { older: Range; newer: Range } | null {
  const saves = savesIn(history, range);
  if (saves.length < 2) return null;
  const middle = saves[Math.ceil(saves.length / 2) - 1];
  if (middle === undefined) return null;
  return { older: { from: range.from, to: middle }, newer: { from: middle, to: range.to } };
}

/// The single save on either side of the one a range ends at, for walking the history one save
/// at a time.
export function neighborsOf(
  history: readonly HistoryEntry[],
  range: Range,
): { older: Range | null; newer: Range | null } {
  const versions = history.map((entry) => entry.version).sort((a, b) => a - b);
  const at = versions.indexOf(range.to);
  const older = at > 0 ? versions[at - 1] : undefined;
  const newer = at >= 0 ? versions[at + 1] : undefined;
  return {
    older: older === undefined ? null : singleSave(history, older),
    newer: newer === undefined ? null : singleSave(history, newer),
  };
}

/// Where a save sits against the range being compared.
export type Placement = 'from' | 'to' | 'inside' | 'outside';

export function placementOf(range: Range, version: number): Placement {
  if (version === range.to) return 'to';
  if (version === range.from) return 'from';
  const low = Math.min(range.from ?? Number.NEGATIVE_INFINITY, range.to);
  const high = Math.max(range.from ?? Number.NEGATIVE_INFINITY, range.to);
  return version > low && version < high ? 'inside' : 'outside';
}

export const CHANGE_LETTER: Record<FileChange, string> = {
  added: 'A',
  removed: 'D',
  modified: 'M',
};

/// The test-fires of each save, newest first, so a save reads as good or bad by what its runs did.
export function launchesByVersion<R extends Pick<PlaybookRun, 'draft_version'>>(
  runs: readonly R[],
): Map<number, R[]> {
  const by = new Map<number, R[]>();
  for (const run of runs) {
    const version = run.draft_version;
    if (version === null || version === undefined) continue;
    const list = by.get(version);
    if (list === undefined) by.set(version, [run]);
    else list.push(run);
  }
  return by;
}

export type DiffLineKind = 'hunk' | 'add' | 'del' | 'context' | 'note';

export interface DiffLine {
  kind: DiffLineKind;
  text: string;
  /// The line's number on each side; null on the side that does not hold it.
  oldLine: number | null;
  newLine: number | null;
}

const HUNK = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/;

/// One file's unified diff as rows. The `---`/`+++` header is dropped; inside a hunk only the
/// first character decides a row's kind, so a removed line that itself starts with `--` stays a
/// removal.
export function parseUnified(unified: string): DiffLine[] {
  const rows: DiffLine[] = [];
  const lines = unified.endsWith('\n') ? unified.slice(0, -1).split('\n') : unified.split('\n');
  let oldLine = 0;
  let newLine = 0;
  let inHunk = false;
  for (const line of lines) {
    const hunk = HUNK.exec(line);
    if (hunk !== null) {
      oldLine = Number(hunk[1]);
      newLine = Number(hunk[2]);
      inHunk = true;
      rows.push({ kind: 'hunk', text: line, oldLine: null, newLine: null });
      continue;
    }
    if (!inHunk) continue;
    const text = line.slice(1);
    switch (line[0]) {
      case '+':
        rows.push({ kind: 'add', text, oldLine: null, newLine });
        newLine += 1;
        break;
      case '-':
        rows.push({ kind: 'del', text, oldLine, newLine: null });
        oldLine += 1;
        break;
      case '\\':
        rows.push({ kind: 'note', text: line.slice(2), oldLine: null, newLine: null });
        break;
      default:
        rows.push({ kind: 'context', text, oldLine, newLine });
        oldLine += 1;
        newLine += 1;
    }
  }
  return rows;
}
