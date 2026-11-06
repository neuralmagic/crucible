import { describe, expect, it } from 'vitest';
import type { components } from '../api/schema';
import { diffLines, inheritsEngineEnv, migrateBody, migrationSummary } from './draftMigration';

type Migration = components['schemas']['DraftMigrationDto'];

const DRY_RUN: Migration = {
  from: 1,
  to: 2,
  base_version: 4,
  dry_run: true,
  diffs: [
    {
      path: 'crucible.toml',
      unified:
        '--- a/crucible.toml\n+++ b/crucible.toml\n@@ -1,3 +1,5 @@\n+edition = 2\n+\n [workflow]\n-type = "x"\n',
    },
  ],
  changes: [
    { kind: 'edition', table: null, entry: null, read_at: [], message: 'edition = 2 (was 1)' },
    {
      kind: 'declared',
      table: '[workflow]',
      entry: 'SLACK_WEBHOOK_URL?',
      read_at: ['scripts/notify.py:3'],
      message: '[workflow].env declares "SLACK_WEBHOOK_URL?", read at scripts/notify.py:3',
    },
  ],
  findings: [
    {
      kind: 'unattributed_reads',
      path: 'tools/backfill.py',
      message: 'tools/backfill.py reads BACKFILL_SINCE, and no host command was found to run it',
    },
  ],
  saved: null,
};

describe('inheritsEngineEnv', () => {
  it('is edition 1 alone', () => {
    expect(inheritsEngineEnv(1)).toBe(true);
    expect(inheritsEngineEnv(2)).toBe(false);
    expect(inheritsEngineEnv(null)).toBe(false);
    expect(inheritsEngineEnv(undefined)).toBe(false);
  });
});

describe('migrateBody', () => {
  it('names the version the migration reads and whether to store it', () => {
    expect(migrateBody(4, true)).toEqual({ base_version: 4, dry_run: true });
    expect(migrateBody(4, false)).toEqual({ base_version: 4, dry_run: false });
  });
});

describe('diffLines', () => {
  it('tells headers, hunks, additions, removals, and context apart', () => {
    expect(diffLines(DRY_RUN.diffs[0].unified).map((l) => l.kind)).toEqual([
      'header',
      'header',
      'hunk',
      'add',
      'add',
      'context',
      'remove',
    ]);
  });

  it('keeps a diff without a trailing newline whole', () => {
    expect(diffLines('+a\n b')).toEqual([
      { text: '+a', kind: 'add' },
      { text: ' b', kind: 'context' },
    ]);
  });
});

describe('migrationSummary', () => {
  it('counts the declarations and the findings', () => {
    expect(migrationSummary(DRY_RUN)).toBe(
      'edition 1 → 2, 1 variable declared, 1 finding to review',
    );
  });

  it('says when there is nothing to declare or review', () => {
    expect(migrationSummary({ ...DRY_RUN, changes: DRY_RUN.changes.slice(0, 1), findings: [] })).toBe(
      'edition 1 → 2, 0 variables declared',
    );
  });

  it('says a draft already at the edition is', () => {
    expect(migrationSummary({ ...DRY_RUN, from: 2, changes: [], diffs: [], findings: [] })).toBe(
      'already at edition 2',
    );
  });
});
