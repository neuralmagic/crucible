// Pure projection helpers for the run report pages: unified-diff parsing and formatting, kept free
// of React so the diff-line classification stays unit-testable in isolation.

import type { components } from '../api/schema';

export type Candidate = components['schemas']['CandidateDto'];

/// Score/cost formatting, consistent with the runs list. The API row carries no gate, so no gate
/// unit (ms / "failed") is appended — a plain number stands in.
export function formatScore(score: number | null | undefined): string {
  return score === null || score === undefined ? '—' : score.toFixed(1);
}

export function formatCost(cost: number | null | undefined): string {
  return cost === null || cost === undefined ? '—' : `$${cost.toFixed(2)}`;
}

/// The chip a run with transport losses wears on every list: tasks the engine never got to run
/// because the infrastructure under them failed, which a `done` status alone hides.
export function transportLossLabel(losses: number | null | undefined): string | null {
  if (!losses || losses <= 0) return null;
  return losses === 1 ? '1 task lost to transport' : `${losses} tasks lost to transport`;
}

/// Human file size for the artifact manifest; null/undefined (an S3 entry the controller can't
/// stat) renders as an em-dash.
export function formatBytes(size: number | null | undefined): string {
  if (size === null || size === undefined) return '—';
  if (size < 1024) return `${size} B`;
  const units = ['KiB', 'MiB', 'GiB'];
  let v = size / 1024;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u += 1;
  }
  return `${v >= 10 ? v.toFixed(0) : v.toFixed(1)} ${units[u]}`;
}

/// Client-side mirror of the server's `model.rs::run_id_created`: run ids lead with a
/// `YYYYMMDDTHHMMSSZ` stamp, returned as RFC3339. If the stamp format ever changes, both parsers
/// move together. `null` for ids without the stamp.
export function runCreatedFromId(runId: string): string | null {
  if (!/^\d{8}T\d{6}Z/.test(runId)) return null;
  const y = runId.slice(0, 4);
  const m = runId.slice(4, 6);
  const d = runId.slice(6, 8);
  const hh = runId.slice(9, 11);
  const mm = runId.slice(11, 13);
  const ss = runId.slice(13, 15);
  return `${y}-${m}-${d}T${hh}:${mm}:${ss}Z`;
}

/// The tone a candidate's decision renders with — mirrors the live feed's `decisionTone`, with
/// `baseline` broken out (the iter-0 measurement, not a real attempt).
export type DecisionTone = 'keep' | 'discard' | 'baseline' | 'neutral';

export function decisionTone(decision: string | null | undefined): DecisionTone {
  const d = (decision ?? '').toLowerCase();
  if (d.includes('baseline')) return 'baseline';
  if (d.includes('keep')) return 'keep';
  if (d.includes('drop') || d.includes('reject') || d.includes('discard')) return 'discard';
  return 'neutral';
}

/// The baseline measurement (iter 0, or a decision tagged `baseline`) rather than a real iteration.
export function isBaseline(c: Candidate): boolean {
  return c.iter === 0 || decisionTone(c.decision) === 'baseline';
}

// --- unified diff ------------------------------------------------------------

export type DiffLineKind = 'ctx' | 'add' | 'del';
export type DiffFileStatus = 'modified' | 'added' | 'deleted' | 'renamed';

export interface DiffLine {
  kind: DiffLineKind;
  oldNo: number | null;
  newNo: number | null;
  text: string;
}

export interface DiffHunk {
  heading: string;
  lines: DiffLine[];
}

export interface DiffFile {
  path: string;
  status: DiffFileStatus;
  renamedFrom: string | null;
  hunks: DiffHunk[];
}

interface HunkHeader {
  oldStart: number;
  newStart: number;
  heading: string;
}

/// Parse a `@@ -old[,len] +new[,len] @@ heading` line, tolerating the count-less terse form
/// (`@@ -1 +1 @@`). `null` for any non-hunk line.
function parseHunkHeader(line: string): HunkHeader | null {
  if (!line.startsWith('@@ ')) return null;
  const rest = line.slice(3);
  const end = rest.indexOf(' @@');
  if (end < 0) return null;
  const ranges = rest.slice(0, end);
  const heading = rest.slice(end + 3).trim();
  const parts = ranges.split(/\s+/).filter((p) => p.length > 0);
  if (parts.length < 2) return null;
  const old = parts[0].startsWith('-') ? parts[0].slice(1) : null;
  const neu = parts[1].startsWith('+') ? parts[1].slice(1) : null;
  if (old === null || neu === null) return null;
  const start = (s: string): number | null => {
    const n = Number.parseInt(s.split(',')[0], 10);
    return Number.isNaN(n) ? null : n;
  };
  const oldStart = start(old);
  const newStart = start(neu);
  if (oldStart === null || newStart === null) return null;
  return { oldStart, newStart, heading };
}

/// Parse a (possibly multi-file) unified `git diff` into [`DiffFile`]s, porting
/// `report.rs::parse_unified`. Tolerant by design: skips metadata it doesn't render (`index`, mode
/// lines, `\ No newline…`) and copes with terse hunk headers. Anything it can't place is dropped
/// rather than throwing.
export function parseUnifiedDiff(diff: string): DiffFile[] {
  const files: DiffFile[] = [];
  let oldNo = 0;
  let newNo = 0;

  for (const line of diff.split('\n')) {
    if (line.startsWith('diff --git ')) {
      const rest = line.slice('diff --git '.length);
      const idx = rest.lastIndexOf(' b/');
      const raw = idx >= 0 ? rest.slice(idx + 3) : rest;
      const path = raw.startsWith('a/') ? raw.slice(2) : raw;
      files.push({ path, status: 'modified', renamedFrom: null, hunks: [] });
      continue;
    }
    const file = files[files.length - 1];
    if (!file) continue; // a stray hunk with no `diff --git` header — nothing to attach it to.

    if (line.startsWith('new file mode')) {
      file.status = 'added';
    } else if (line.startsWith('deleted file mode')) {
      file.status = 'deleted';
    } else if (line.startsWith('rename from ')) {
      file.status = 'renamed';
      file.renamedFrom = line.slice('rename from '.length);
    } else if (line.startsWith('+++ b/')) {
      file.path = line.slice('+++ b/'.length);
    } else if (line.startsWith('+++ ') || line.startsWith('--- ') || line.startsWith('index ')) {
      // Header noise we don't render (path comes from `diff --git` / `+++ b/`).
    } else {
      const header = parseHunkHeader(line);
      if (header) {
        oldNo = header.oldStart;
        newNo = header.newStart;
        file.hunks.push({ heading: header.heading, lines: [] });
        continue;
      }
      const hunk = file.hunks[file.hunks.length - 1];
      if (!hunk) continue; // body lines only count once we're inside a hunk.
      const head = line.charAt(0);
      if (head === '+') {
        hunk.lines.push({ kind: 'add', oldNo: null, newNo, text: line.slice(1) });
        newNo += 1;
      } else if (head === '-') {
        hunk.lines.push({ kind: 'del', oldNo, newNo: null, text: line.slice(1) });
        oldNo += 1;
      } else if (head === '\\') {
        // "\ No newline at end of file" — not a real line.
      } else {
        const text = line.startsWith(' ') ? line.slice(1) : line;
        hunk.lines.push({ kind: 'ctx', oldNo, newNo, text });
        oldNo += 1;
        newNo += 1;
      }
    }
  }
  return files;
}

/// Whether a lower score is the better one, inferred from where the run's best score sits in the
/// spread of everything it measured. Domains disagree on direction and nothing on the wire says
/// which way this metric runs.
export function scoresImproveDownward(scores: readonly number[], best: number | null | undefined): boolean {
  if (best === null || best === undefined || scores.length === 0) return true;
  const lo = Math.min(...scores);
  const hi = Math.max(...scores);
  return Math.abs(best - lo) <= Math.abs(best - hi);
}
