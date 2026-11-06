import type { components } from '../api/schema';

type Migration = components['schemas']['DraftMigrationDto'];
type FindingKind = components['schemas']['MigrationFindingKind'];

/// Edition 1 is the one whose host commands inherit the engine's environment. A manifest the
/// engine could not read for an edition (`null`) says nothing either way.
export function inheritsEngineEnv(edition: number | null | undefined): boolean {
  return edition === 1;
}

export function migrateBody(
  baseVersion: number,
  dryRun: boolean,
): { base_version: number; dry_run: boolean } {
  return { base_version: baseVersion, dry_run: dryRun };
}

export const FINDING_LABEL: Readonly<Record<FindingKind, string>> = {
  unresolved_command: 'unread command',
  unattributed_reads: 'unattributed',
  not_affected: 'not affected',
  computed_read: 'computed name',
};

export type DiffLineKind = 'header' | 'hunk' | 'add' | 'remove' | 'context';

/// A unified diff's lines, each with what it is, so additions and removals can be told apart.
export function diffLines(unified: string): { text: string; kind: DiffLineKind }[] {
  const lines = unified.endsWith('\n') ? unified.slice(0, -1).split('\n') : unified.split('\n');
  return lines.map((text) => {
    if (text.startsWith('+++') || text.startsWith('---')) return { text, kind: 'header' };
    if (text.startsWith('@@')) return { text, kind: 'hunk' };
    if (text.startsWith('+')) return { text, kind: 'add' };
    if (text.startsWith('-')) return { text, kind: 'remove' };
    return { text, kind: 'context' };
  });
}

/// One line saying what a migration would do: the edition move, how many variables it declares,
/// and how many findings need a reviewer.
export function migrationSummary(m: Migration): string {
  if (m.changes.length === 0) return `already at edition ${m.from}`;
  const declared = m.changes.filter((c) => c.kind === 'declared').length;
  const parts = [`edition ${m.from} → ${m.to}`];
  parts.push(declared === 1 ? '1 variable declared' : `${declared} variables declared`);
  if (m.findings.length > 0) {
    parts.push(m.findings.length === 1 ? '1 finding to review' : `${m.findings.length} findings to review`);
  }
  return parts.join(', ');
}
