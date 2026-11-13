import { describe, expect, it } from 'vitest';
import {
  halvesOf,
  launchesByVersion,
  neighborsOf,
  parseUnified,
  placementOf,
  rangeLabel,
  rangeOf,
  rangeSearch,
  savesIn,
  singleSave,
  type HistoryEntry,
} from './draftHistory';

function entry(version: number, previous: number | null): HistoryEntry {
  return {
    version,
    previous_version: previous,
    tree_digest: null,
    schema_digest: 'sha256:aa',
    diagnostics: 0,
    core_rev: '7c2c1a5',
    created_by: 'wren',
    created_at: '2026-08-22T09:00:00Z',
    changes: [],
  };
}

/// Versions 1, 2, 3, 5, 6, 7: newest first, with a gap where a save was never stored.
const HISTORY = [entry(7, 6), entry(6, 5), entry(5, 3), entry(3, 2), entry(2, 1), entry(1, null)];

function params(search: string): URLSearchParams {
  return new URLSearchParams(search);
}

describe('rangeOf', () => {
  it('defaults to the newest save against the one before it', () => {
    expect(rangeOf(params(''), HISTORY)).toEqual({ from: 6, to: 7 });
  });

  it('takes the save before `to` when `from` is absent', () => {
    expect(rangeOf(params('to=5'), HISTORY)).toEqual({ from: 3, to: 5 });
    expect(rangeOf(params('to=1'), HISTORY)).toEqual({ from: null, to: 1 });
  });

  it('reads any two saves, in either order', () => {
    expect(rangeOf(params('from=2&to=6'), HISTORY)).toEqual({ from: 2, to: 6 });
    expect(rangeOf(params('from=6&to=2'), HISTORY)).toEqual({ from: 6, to: 2 });
  });

  it('ignores a version the draft does not hold', () => {
    expect(rangeOf(params('from=4&to=99'), HISTORY)).toEqual({ from: 6, to: 7 });
    expect(rangeOf(params('from=x&to=-3'), HISTORY)).toEqual({ from: 6, to: 7 });
  });

  it('is null while the draft has no saves', () => {
    expect(rangeOf(params('to=1'), [])).toBeNull();
  });
});

describe('rangeSearch', () => {
  it('round-trips through rangeOf', () => {
    for (const range of [
      { from: 2, to: 6 },
      { from: 6, to: 2 },
      { from: null, to: 1 },
    ]) {
      expect(rangeOf(params(rangeSearch(range)), HISTORY)).toEqual(range);
    }
  });
});

describe('rangeLabel', () => {
  it('names the empty pack the first save is compared against', () => {
    expect(rangeLabel({ from: 2, to: 6 })).toBe('v2 → v6');
    expect(rangeLabel({ from: null, to: 1 })).toBe('empty → v1');
  });
});

describe('savesIn', () => {
  it('is every save after `from` through `to`', () => {
    expect(savesIn(HISTORY, { from: 2, to: 6 })).toEqual([3, 5, 6]);
    expect(savesIn(HISTORY, { from: null, to: 2 })).toEqual([1, 2]);
  });

  it('is empty for a backward or same-save range', () => {
    expect(savesIn(HISTORY, { from: 6, to: 2 })).toEqual([]);
    expect(savesIn(HISTORY, { from: 3, to: 3 })).toEqual([]);
  });
});

describe('halvesOf', () => {
  it('splits a range at its middle save', () => {
    expect(halvesOf(HISTORY, { from: 1, to: 7 })).toEqual({
      older: { from: 1, to: 5 },
      newer: { from: 5, to: 7 },
    });
    expect(halvesOf(HISTORY, { from: 5, to: 7 })).toEqual({
      older: { from: 5, to: 6 },
      newer: { from: 6, to: 7 },
    });
  });

  it('has nothing to halve in a single save or a backward range', () => {
    expect(halvesOf(HISTORY, { from: 6, to: 7 })).toBeNull();
    expect(halvesOf(HISTORY, { from: null, to: 1 })).toBeNull();
    expect(halvesOf(HISTORY, { from: 7, to: 1 })).toBeNull();
  });
});

describe('neighborsOf', () => {
  it('steps one save either way from where the range ends', () => {
    expect(neighborsOf(HISTORY, { from: 1, to: 5 })).toEqual({
      older: { from: 2, to: 3 },
      newer: { from: 5, to: 6 },
    });
  });

  it('stops at the first and the newest save', () => {
    expect(neighborsOf(HISTORY, singleSave(HISTORY, 1)).older).toBeNull();
    expect(neighborsOf(HISTORY, singleSave(HISTORY, 7)).newer).toBeNull();
  });
});

describe('placementOf', () => {
  it('marks both ends and what lies between them, whichever way the range runs', () => {
    for (const range of [
      { from: 2, to: 6 },
      { from: 6, to: 2 },
    ]) {
      expect(placementOf(range, range.from)).toBe('from');
      expect(placementOf(range, range.to)).toBe('to');
      expect(placementOf(range, 3)).toBe('inside');
      expect(placementOf(range, 7)).toBe('outside');
      expect(placementOf(range, 1)).toBe('outside');
    }
  });

  it('marks only the first save when it is compared against the empty pack', () => {
    expect(placementOf({ from: null, to: 1 }, 1)).toBe('to');
    expect(placementOf({ from: null, to: 1 }, 2)).toBe('outside');
  });
});

describe('launchesByVersion', () => {
  it('groups test-fires under the save they froze and drops registered launches', () => {
    const by = launchesByVersion([
      { key: 'playbook:studio:c', draft_version: 3 },
      { key: 'playbook:studio:b', draft_version: 2 },
      { key: 'playbook:studio:a', draft_version: 3 },
      { key: 'playbook:survey:z', draft_version: null },
    ]);
    expect([...by.keys()]).toEqual([3, 2]);
    expect(by.get(3)?.map((run) => run.key)).toEqual(['playbook:studio:c', 'playbook:studio:a']);
  });
});

describe('parseUnified', () => {
  it('numbers each side and drops the file header', () => {
    const rows = parseUnified(
      '--- a/workflow.star\n+++ b/workflow.star\n@@ -2,3 +2,3 @@\n keep\n-old\n+new\n tail\n',
    );
    expect(rows).toEqual([
      { kind: 'hunk', text: '@@ -2,3 +2,3 @@', oldLine: null, newLine: null },
      { kind: 'context', text: 'keep', oldLine: 2, newLine: 2 },
      { kind: 'del', text: 'old', oldLine: 3, newLine: null },
      { kind: 'add', text: 'new', oldLine: null, newLine: 3 },
      { kind: 'context', text: 'tail', oldLine: 4, newLine: 4 },
    ]);
  });

  it('restarts the numbering at every hunk', () => {
    const rows = parseUnified('--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n@@ -40,2 +40,3 @@\n x\n+y\n z\n');
    expect(rows.filter((r) => r.kind === 'add').map((r) => r.newLine)).toEqual([1, 41]);
    expect(rows.at(-1)).toEqual({ kind: 'context', text: 'z', oldLine: 41, newLine: 42 });
  });

  it('keeps a changed line that looks like a file header as a change', () => {
    const rows = parseUnified('--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n--- a rule\n+++ b rule\n same\n');
    expect(rows.slice(1)).toEqual([
      { kind: 'del', text: '-- a rule', oldLine: 1, newLine: null },
      { kind: 'add', text: '++ b rule', oldLine: null, newLine: 1 },
      { kind: 'context', text: 'same', oldLine: 2, newLine: 2 },
    ]);
  });

  it('reads an added file against /dev/null and the missing-newline note', () => {
    const rows = parseUnified(
      '--- /dev/null\n+++ b/notes.md\n@@ -0,0 +1,2 @@\n+one\n+two\n\\ No newline at end of file\n',
    );
    expect(rows).toEqual([
      { kind: 'hunk', text: '@@ -0,0 +1,2 @@', oldLine: null, newLine: null },
      { kind: 'add', text: 'one', oldLine: null, newLine: 1 },
      { kind: 'add', text: 'two', oldLine: null, newLine: 2 },
      { kind: 'note', text: 'No newline at end of file', oldLine: null, newLine: null },
    ]);
  });

  it('is empty for an empty diff', () => {
    expect(parseUnified('')).toEqual([]);
  });
});
