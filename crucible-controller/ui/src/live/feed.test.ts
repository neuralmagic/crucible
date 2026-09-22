import { describe, expect, it } from 'vitest';
import {
  ALL_CATEGORIES,
  appendLine,
  appendPodLine,
  atBottom,
  describeLine,
  foldSessionText,
  rowCategory,
  rowMatches,
  scorePoints,
  type FeedRow,
} from './feed';
import { parseSessionLine } from './session';

const line = (json: string) => parseSessionLine(json);
const row = (id: number, json: string): FeedRow => ({ id, kind: 'line', line: line(json) });

describe('appendLine', () => {
  it('coalesces consecutive text deltas into one growing block', () => {
    let rows: FeedRow[] = [];
    rows = appendLine(rows, 1, line('{"kind":"agent","event":{"kind":"text","delta":"hel"}}'), 10);
    rows = appendLine(rows, 2, line('{"kind":"agent","event":{"kind":"text","delta":"lo"}}'), 10);
    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({ id: 1, kind: 'text', text: 'hello' });
  });

  it('breaks coalescing when a non-delta line interleaves', () => {
    let rows: FeedRow[] = [];
    rows = appendLine(rows, 1, line('{"kind":"agent","event":{"kind":"text","delta":"a"}}'), 10);
    rows = appendLine(rows, 2, line('{"kind":"note","msg":"n"}'), 10);
    rows = appendLine(rows, 3, line('{"kind":"agent","event":{"kind":"text","delta":"b"}}'), 10);
    expect(rows.map((r) => r.kind)).toEqual(['text', 'line', 'text']);
  });

  it('keeps thinking and text streams as separate blocks', () => {
    let rows: FeedRow[] = [];
    rows = appendLine(rows, 1, line('{"kind":"agent","event":{"kind":"thinking","delta":"hm"}}'), 10);
    rows = appendLine(rows, 2, line('{"kind":"agent","event":{"kind":"text","delta":"so"}}'), 10);
    expect(rows.map((r) => r.kind)).toEqual(['thinking', 'text']);
  });

  it('caps the buffer dropping oldest rows', () => {
    let rows: FeedRow[] = [];
    for (let i = 1; i <= 5; i++) {
      rows = appendLine(rows, i, line(`{"kind":"note","msg":"m${i}"}`), 3);
    }
    expect(rows).toHaveLength(3);
    expect(rows[0].id).toBe(3);
  });

  it('never mutates the input array', () => {
    const first = appendLine([], 1, line('{"kind":"note","msg":"a"}'), 10);
    const snapshot = [...first];
    appendLine(first, 2, line('{"kind":"agent","event":{"kind":"text","delta":"x"}}'), 10);
    expect(first).toEqual(snapshot);
  });
});

describe('foldSessionText', () => {
  it('folds a whole NDJSON transcript, coalescing deltas exactly like appendLine', () => {
    const text = [
      '{"kind":"note","msg":"round 1: propose turn"}',
      '{"kind":"agent","event":{"kind":"text","delta":"drafting "}}',
      '{"kind":"agent","event":{"kind":"text","delta":"the pack"}}',
      '{"kind":"agent","event":{"kind":"result","subtype":"success","turns":3,"cost_usd":0.42}}',
      '',
    ].join('\n');
    const rows = foldSessionText(text, 100);
    expect(rows.map((r) => r.kind)).toEqual(['line', 'text', 'line']);
    expect(rows[1]).toMatchObject({ kind: 'text', text: 'drafting the pack' });
  });

  it('skips blank lines and surfaces garbage as unknown rows', () => {
    const rows = foldSessionText('\n\nnot json at all\n', 100);
    expect(rows).toHaveLength(1);
    const only = rows[0];
    expect(only.kind).toBe('line');
    if (only.kind === 'line') {
      expect(only.line.kind).toBe('unknown');
    }
  });

  it('caps to the newest rows', () => {
    const text = Array.from({ length: 10 }, (_v, i) => `{"kind":"note","msg":"m${i}"}`).join('\n');
    const rows = foldSessionText(text, 3);
    expect(rows).toHaveLength(3);
    expect(rows[0]).toMatchObject({ kind: 'line' });
    const first = rows[0];
    if (first.kind === 'line' && first.line.kind === 'note') {
      expect(first.line.msg).toBe('m7');
    } else {
      throw new Error('expected a note row');
    }
  });

  it('returns no rows for an empty transcript', () => {
    expect(foldSessionText('', 100)).toEqual([]);
  });
});

describe('atBottom', () => {
  it('is true within the threshold and false beyond it', () => {
    expect(atBottom(960, 1000, 40)).toBe(true); // exactly at bottom
    expect(atBottom(930, 1000, 40)).toBe(true); // 30px up, inside default 40
    expect(atBottom(900, 1000, 40)).toBe(false); // 60px up
  });
});

describe('describeLine tones', () => {
  it('classifies keep/drop/baseline candidate decisions', () => {
    expect(describeLine(line('{"kind":"row","row":{"iter":1,"decision":"keep","score":1}}')).tone).toBe('keep');
    expect(describeLine(line('{"kind":"row","row":{"iter":1,"decision":"wide-drop-2"}}')).tone).toBe('drop');
    expect(describeLine(line('{"kind":"row","row":{"iter":0,"decision":"baseline"}}')).tone).toBe('info');
  });

  it('falls back to a raw row for unknown kinds instead of dropping them', () => {
    const view = describeLine(line('{"kind":"zorp","x":1}'));
    expect(view.tag).toBe('raw');
    expect(view.title).toContain('zorp');
  });
});

describe('rowCategory / rowMatches / scorePoints', () => {
  it('buckets rows into the three filter categories', () => {
    expect(rowCategory({ id: 1, kind: 'text', text: 'x' })).toBe('agent');
    expect(rowCategory(row(2, '{"kind":"agent_start","iter":1}'))).toBe('agent');
    expect(rowCategory(row(3, '{"kind":"row","row":{"iter":1,"decision":"keep"}}'))).toBe('candidates');
    expect(rowCategory(row(4, '{"kind":"budget","spent":1,"elapsed_secs":2}'))).toBe('lifecycle');
    expect(ALL_CATEGORIES).toHaveLength(3);
  });

  it('matches case-insensitively over text, tag, title and detail', () => {
    const r = row(1, '{"kind":"row","row":{"iter":3,"decision":"keep","score":1,"note":"Sharper GATE"}}');
    expect(rowMatches(r, 'gate')).toBe(true);
    expect(rowMatches(r, 'KEEP')).toBe(true);
    expect(rowMatches(r, 'zebra')).toBe(false);
    expect(rowMatches(r, '   ')).toBe(true); // blank query matches everything
  });

  it('extracts sparkline points from scored candidate rows only, baselines kept', () => {
    const rows: FeedRow[] = [
      row(1, '{"kind":"row","row":{"iter":0,"decision":"baseline","score":350}}'),
      { id: 2, kind: 'text', text: 'noise' },
      row(3, '{"kind":"row","row":{"iter":1,"decision":"keep","score":290}}'),
      row(4, '{"kind":"row","row":{"iter":2,"decision":"reject","score":400}}'),
      row(5, '{"kind":"row","row":{"iter":3,"decision":"keep"}}'), // unscored: skipped
    ];
    const pts = scorePoints(rows);
    expect(pts.map((p) => [p.score, p.kept])).toEqual([
      [350, true],
      [290, true],
      [400, false],
    ]);
  });
});

describe('appendPodLine', () => {
  it('keeps consecutive pod lines as separate rows', () => {
    // Unlike streamed agent text, pod output is line-oriented: coalescing it would run a stack
    // trace and the line after it into one block.
    let rows = appendPodLine([], 1, 'Trying to pull ghcr.io/x/sandbox:latest...', 10);
    rows = appendPodLine(rows, 2, 'Successfully tagged', 10);
    expect(rows).toEqual([
      { id: 1, kind: 'pod', text: 'Trying to pull ghcr.io/x/sandbox:latest...' },
      { id: 2, kind: 'pod', text: 'Successfully tagged' },
    ]);
  });

  it('caps the buffer like the session feed does', () => {
    let rows: FeedRow[] = [];
    for (let i = 0; i < 5; i += 1) rows = appendPodLine(rows, i, `line ${i}`, 3);
    expect(rows.map((r) => r.id)).toEqual([2, 3, 4]);
  });

  it('never mutates the array it is given', () => {
    const before: FeedRow[] = [{ id: 1, kind: 'pod', text: 'first' }];
    const after = appendPodLine(before, 2, 'second', 10);
    expect(before).toHaveLength(1);
    expect(after).toHaveLength(2);
  });

  it('files pod output under lifecycle and searches its text', () => {
    const row: FeedRow = { id: 1, kind: 'pod', text: 'ImagePullBackOff' };
    expect(rowCategory(row)).toBe('lifecycle');
    expect(rowMatches(row, 'imagepull')).toBe(true);
    expect(rowMatches(row, 'nothing here')).toBe(false);
  });
});
