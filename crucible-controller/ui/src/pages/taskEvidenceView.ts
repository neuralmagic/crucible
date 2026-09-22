// What the node panel reads off `GET /api/runs/{run_id}/tasks/{task}/evidence` and
// `GET /api/runs/{run_id}/log`. Pure functions so the wording is testable without a graph.

import type { components } from '../api/schema';

export type TaskEvidence = components['schemas']['TaskEvidenceDto'];
export type EvidenceFile = components['schemas']['EvidenceFileDto'];
export type RunLog = components['schemas']['RunLogDto'];

/// A file's size as the panel says it: bytes below a kilobyte, one decimal above.
export function fileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/// One file's line under its name: its size, plus why its content is not here when it isn't.
export function fileLine(file: EvidenceFile): string {
  const size = fileSize(file.size_bytes);
  return (file.content ?? null) === null ? `${size} · not shown inline` : size;
}

/// The task's payload, pretty-printed; null when it emitted none.
export function payloadText(evidence: TaskEvidence): string | null {
  const payload = evidence.payload ?? null;
  return payload === null ? null : JSON.stringify(payload, null, 2);
}

/// What the panel says about a task with no terminal result: still running, or nothing recorded.
export function pendingLine(evidence: TaskEvidence): string | null {
  if ((evidence.status ?? null) !== null) return null;
  return evidence.running ? 'running — no result yet' : 'nothing recorded for this task';
}

/// The log block's body: the engine output when this deployment holds it, or where it lives.
export function logBody(log: RunLog): string {
  const text = log.text ?? '';
  if (text !== '') return log.truncated ? `… earlier output dropped\n${text}` : text;
  return log.location ?? 'no output recorded for this run';
}

/// The path the log body is read under, which is what routes it to the editor: real output earns
/// the find widget and line numbers, a location hint stays prose. Plain `.log` in both dispatches,
/// because a pod run's session is NDJSON and the JSON grammar would mark every line after the
/// first as a syntax error.
export function logPath(log: RunLog): string | null {
  return (log.text ?? '') === '' ? null : 'run.log';
}

export type EvidenceKind = 'result' | 'payload' | 'file' | 'log';

/// One row of the evidence list, and what the pane beside it reads. A body of null is a file the
/// run captured but did not carry inline.
export interface EvidenceItem {
  id: string;
  kind: EvidenceKind;
  name: string;
  /// The right-hand line on the row: a file's size, or nothing.
  note: string | null;
  /// The path the body is highlighted by; null for prose read as text.
  path: string | null;
  body: string | null;
}

/// The evidence one task left, as the list reads it: its note, what it emitted, then every file it
/// captured. The run log is not here, because it belongs to the run rather than the task.
export function evidenceItems(evidence: TaskEvidence): EvidenceItem[] {
  const items: EvidenceItem[] = [];
  const note = evidence.note ?? '';
  if (note !== '') {
    items.push({ id: 'result', kind: 'result', name: 'result', note: null, path: null, body: note });
  }
  const payload = payloadText(evidence);
  if (payload !== null) {
    items.push({
      id: 'payload',
      kind: 'payload',
      name: 'output',
      note: null,
      path: `${evidence.task}/payload.json`,
      body: payload,
    });
  }
  for (const file of evidence.files) {
    items.push({
      id: `file:${file.name}`,
      kind: 'file',
      name: file.name,
      note: fileLine(file),
      path: file.name,
      body: file.content ?? null,
    });
  }
  return items;
}
