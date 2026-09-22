import { describe, expect, it } from 'vitest';
import {
  decisionTone,
  formatBytes,
  formatCost,
  formatScore,
  isBaseline,
  parseUnifiedDiff,
  runCreatedFromId,
  transportLossLabel,
  type Candidate,
} from './runReport';

const cand = (iter: number, decision: string, score: number | null): Candidate => ({
  run_id: 'r',
  iter,
  decision,
  score,
});

describe('formatting + tones', () => {
  it('formats scores and costs with em-dash absents', () => {
    expect(formatScore(234.567)).toBe('234.6');
    expect(formatScore(null)).toBe('—');
    expect(formatCost(12.5)).toBe('$12.50');
    expect(formatCost(undefined)).toBe('—');
  });

  it('formats byte sizes with binary units', () => {
    expect(formatBytes(null)).toBe('—');
    expect(formatBytes(undefined)).toBe('—');
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(512)).toBe('512 B');
    expect(formatBytes(2048)).toBe('2.0 KiB');
    expect(formatBytes(15 * 1024)).toBe('15 KiB');
    expect(formatBytes(3 * 1024 * 1024)).toBe('3.0 MiB');
    expect(formatBytes(5 * 1024 * 1024 * 1024)).toBe('5.0 GiB');
    expect(formatBytes(6000 * 1024 * 1024 * 1024)).toBe('6000 GiB');
  });

  it('classifies decisions and baselines', () => {
    expect(decisionTone('keep')).toBe('keep');
    expect(decisionTone('wide-drop-1')).toBe('discard');
    expect(isBaseline(cand(0, 'whatever', 1))).toBe(true);
    expect(isBaseline(cand(3, 'baseline', 1))).toBe(true);
    expect(isBaseline(cand(3, 'keep', 1))).toBe(false);
  });
});

describe('runCreatedFromId', () => {
  it('parses a stamped run id to RFC3339', () => {
    expect(runCreatedFromId('20260812T031500Z-vllm-abc')).toBe('2026-08-12T03:15:00Z');
    expect(runCreatedFromId('20260812T031500Z')).toBe('2026-08-12T03:15:00Z');
  });

  it('rejects ids without a leading stamp', () => {
    expect(runCreatedFromId('')).toBeNull();
    expect(runCreatedFromId('20260812T03150')).toBeNull(); // too short
    expect(runCreatedFromId('20260812X031500Z')).toBeNull(); // missing T
    expect(runCreatedFromId('20260812T031500X')).toBeNull(); // missing Z
    expect(runCreatedFromId('2026o812T031500Z')).toBeNull(); // non-digit date
    expect(runCreatedFromId('20260812T03i500Z')).toBeNull(); // non-digit time
    expect(runCreatedFromId('run-20260812T031500Z')).toBeNull(); // stamp not leading
  });
});

describe('parseUnifiedDiff', () => {
  const DIFF = `diff --git a/src/lib.rs b/src/lib.rs
index 111..222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@ mod header
 fn a() {}
-fn b() {}
+fn b2() {}
+fn c() {}
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
--- a/gone.rs
+++ /dev/null
@@ -1,1 +0,0 @@
-goodbye
`;

  it('splits files, tracks status, and numbers both sides', () => {
    const files = parseUnifiedDiff(DIFF);
    expect(files).toHaveLength(2);
    expect(files[0]).toMatchObject({ path: 'src/lib.rs', status: 'modified' });
    expect(files[1].status).toBe('deleted');

    const lines = files[0].hunks[0].lines;
    expect(lines.map((l) => l.kind)).toEqual(['ctx', 'del', 'add', 'add']);
    const added = lines.filter((l) => l.kind === 'add');
    expect(added[0]).toMatchObject({ oldNo: null, newNo: 2 });
    expect(added[1]).toMatchObject({ oldNo: null, newNo: 3 });
    const deleted = lines.find((l) => l.kind === 'del');
    expect(deleted).toMatchObject({ oldNo: 2, newNo: null });
  });

  it('tolerates garbage without throwing and drops orphan hunks', () => {
    expect(parseUnifiedDiff('')).toEqual([]);
    expect(parseUnifiedDiff('@@ -1 +1 @@\n+orphan\n')).toEqual([]);
    expect(() => parseUnifiedDiff('random\ntext\n')).not.toThrow();
  });
});

describe('transportLossLabel', () => {
  it('is null when nothing was lost and counts otherwise', () => {
    expect(transportLossLabel(0)).toBeNull();
    expect(transportLossLabel(null)).toBeNull();
    expect(transportLossLabel(undefined)).toBeNull();
    expect(transportLossLabel(1)).toBe('1 task lost to transport');
    expect(transportLossLabel(3)).toBe('3 tasks lost to transport');
  });
});
