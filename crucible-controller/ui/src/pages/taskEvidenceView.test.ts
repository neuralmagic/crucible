import { describe, expect, it } from 'vitest';
import {
  evidenceItems,
  fileLine,
  fileSize,
  logBody,
  logPath,
  payloadText,
  pendingLine,
} from './taskEvidenceView';
import type { TaskEvidence } from './taskEvidenceView';

function evidence(overrides: Partial<TaskEvidence> = {}): TaskEvidence {
  return {
    run_id: 'RUN-0412',
    task: 'triage[1027]',
    status: 'pass',
    iter: 0,
    note: '',
    attempts: 1,
    cost_usd: 0.23,
    secs: 12,
    payload: null,
    files: [],
    running: false,
    ...overrides,
  };
}

describe('a task file', () => {
  it('reads its size in the unit that fits', () => {
    expect(fileSize(512)).toBe('512 B');
    expect(fileSize(1379)).toBe('1.3 KB');
    expect(fileSize(8 * 1024 * 1024)).toBe('8.0 MB');
  });

  it('says why a body is missing rather than showing an empty block', () => {
    expect(fileLine({ name: 'TRIAGE.md', size_bytes: 1379, content: '# 1027\n' })).toBe('1.3 KB');
    expect(fileLine({ name: 'core.bin', size_bytes: 2048, content: null })).toBe(
      '2.0 KB · not shown inline',
    );
  });
});

describe('a task payload', () => {
  it('is pretty-printed, and absent when the task emitted none', () => {
    expect(payloadText(evidence({ payload: { severity: 'low' } }))).toBe(
      '{\n  "severity": "low"\n}',
    );
    expect(payloadText(evidence())).toBeNull();
  });
});

describe('a task with no result', () => {
  it('says it is running rather than reading as finished with nothing to show', () => {
    expect(pendingLine(evidence({ status: null, running: true }))).toBe('running — no result yet');
    expect(pendingLine(evidence({ status: null, running: false }))).toBe(
      'nothing recorded for this task',
    );
    expect(pendingLine(evidence())).toBeNull();
  });
});

describe('the run log', () => {
  it('marks a dropped head and falls back to where the output lives', () => {
    expect(logBody({ run_id: 'r', dispatch: 'local', text: 'done\n', truncated: false, location: null })).toBe(
      'done\n',
    );
    expect(
      logBody({ run_id: 'r', dispatch: 'local', text: 'tail\n', truncated: true, location: null }),
    ).toBe('… earlier output dropped\ntail\n');
    expect(
      logBody({
        run_id: 'r',
        dispatch: 'pod',
        text: null,
        truncated: false,
        location: 'pod loop-abc in namespace autoresearch',
      }),
    ).toBe('pod loop-abc in namespace autoresearch');
    expect(logBody({ run_id: 'r', dispatch: 'pod', text: null, truncated: false, location: null })).toBe(
      'no output recorded for this run',
    );
  });

  it('sends real output to the editor and a location hint to prose', () => {
    expect(
      logPath({ run_id: 'r', dispatch: 'pod', text: '{"task":"scan"}\n', truncated: false, location: null }),
    ).toBe('run.log');
    expect(
      logPath({
        run_id: 'r',
        dispatch: 'pod',
        text: null,
        truncated: false,
        location: 'pod loop-abc in namespace crucible-system on cluster wharf',
      }),
    ).toBeNull();
  });
});

describe('the evidence list', () => {
  it('reads the note, the payload, then every captured file', () => {
    const items = evidenceItems(
      evidence({
        note: 'classified as a feature',
        payload: { severity: 'low' },
        files: [
          { name: 'TRIAGE.md', size_bytes: 1379, content: '# 1027\n' },
          { name: 'core.bin', size_bytes: 2048, content: null },
        ],
      }),
    );

    expect(items.map((item) => item.id)).toEqual([
      'result',
      'payload',
      'file:TRIAGE.md',
      'file:core.bin',
    ]);
    expect(items[0]).toMatchObject({ name: 'result', path: null, body: 'classified as a feature' });
    expect(items[1]).toMatchObject({ path: 'triage[1027]/payload.json', body: '{\n  "severity": "low"\n}' });
    expect(items[2]).toMatchObject({ note: '1.3 KB', path: 'TRIAGE.md', body: '# 1027\n' });
    expect(items[3]).toMatchObject({ note: '2.0 KB · not shown inline', body: null });
  });

  it('leaves out what a task never produced', () => {
    expect(evidenceItems(evidence())).toEqual([]);
  });
});
