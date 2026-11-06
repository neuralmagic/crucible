import { useState } from 'react';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import type { components } from '../api/schema';
import { Button, Mono, Section, SectionBody, SectionHeader } from '../ui';
import { FormError, Note, Notice } from './formControls';
import {
  diffLines,
  FINDING_LABEL,
  inheritsEngineEnv,
  migrateBody,
  migrationSummary,
  type DiffLineKind,
} from './draftMigration';

type Migration = components['schemas']['DraftMigrationDto'];

const LINE_TONE: Readonly<Record<DiffLineKind, string>> = {
  header: 'text-ink-3',
  hunk: 'text-ink-3',
  add: 'text-green',
  remove: 'text-red',
  context: 'text-ink-2',
};

export interface MigrateDraftSectionProps {
  id: string;
  /// The edition the stored manifest declares.
  edition: number | null | undefined;
  /// The stored version the migration reads and saves over.
  baseVersion: number;
  writable: boolean;
  /// The editor holds edits the stored version does not.
  dirty: boolean;
  /// Reload the studio onto the version the migration stored.
  onMigrated: () => Promise<void>;
}

/// The edition-1 banner, and moving the draft to edition 2: a dry run first, showing the diff and
/// what needs review, then the same migration saved as the next version.
export function MigrateDraftSection({
  id,
  edition,
  baseVersion,
  writable,
  dirty,
  onMigrated,
}: MigrateDraftSectionProps) {
  const migrate = $api.useMutation('post', '/api/playbook-drafts/{id}/migrate');
  const [preview, setPreview] = useState<Migration | null>(null);
  const [error, setError] = useState<string | null>(null);

  if (!inheritsEngineEnv(edition)) return null;

  const run = async (dryRun: boolean) => {
    setError(null);
    try {
      const migrated = await migrate.mutateAsync({
        params: { path: { id } },
        body: migrateBody(baseVersion, dryRun),
      });
      if (dryRun) {
        setPreview(migrated);
        return;
      }
      setPreview(null);
      await onMigrated();
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  return (
    <Section>
      <Notice label="Edition 1">
        This draft is edition 1, so its workflow command and evaluate tasks inherit the
        engine&apos;s whole environment, credentials included. Edition 2 gives them only what the
        manifest declares.
      </Notice>
      <SectionBody>
        <div className="grid gap-2" data-testid="draft-migrate">
          {writable && preview === null ? (
            <span>
              <Button
                className="uppercase"
                disabled={migrate.isPending}
                onClick={() => {
                  void run(true);
                }}
              >
                {migrate.isPending ? 'Reading the pack…' : 'Migrate to edition 2'}
              </Button>
            </span>
          ) : null}
          {preview === null ? null : (
            <MigrationPreview
              preview={preview}
              applying={migrate.isPending}
              dirty={dirty}
              onApply={() => {
                void run(false);
              }}
              onCancel={() => {
                setPreview(null);
              }}
            />
          )}
          {error === null ? null : <FormError>{error}</FormError>}
        </div>
      </SectionBody>
    </Section>
  );
}

interface MigrationPreviewProps {
  preview: Migration;
  applying: boolean;
  dirty: boolean;
  onApply: () => void;
  onCancel: () => void;
}

function MigrationPreview({ preview, applying, dirty, onApply, onCancel }: MigrationPreviewProps) {
  return (
    <div className="grid gap-2" data-testid="draft-migrate-preview">
      <SectionHeader
        title="Migration"
        actions={
          <Mono size="data" tone="ink-3">
            {migrationSummary(preview)}
          </Mono>
        }
      />
      {preview.diffs.map((diff) => (
        <pre
          key={diff.path}
          className="m-0 overflow-x-auto border border-rule bg-sunk px-3 py-2 font-mono text-data"
          data-testid="draft-migrate-diff"
        >
          {diffLines(diff.unified).map((line, index) => (
            <span key={`${diff.path}-${index}`} className={`block ${LINE_TONE[line.kind]}`}>
              {line.text === '' ? ' ' : line.text}
            </span>
          ))}
        </pre>
      ))}
      <ul className="m-0 grid list-none gap-1 p-0" data-testid="draft-migrate-changes">
        {preview.changes.map((change) => (
          <li key={change.message} className="font-mono text-data text-ink-2">
            {change.message}
          </li>
        ))}
      </ul>
      {preview.findings.length === 0 ? (
        <Note>Nothing needs review: every host command&apos;s code was read.</Note>
      ) : (
        <ul className="m-0 grid list-none gap-1 p-0" data-testid="draft-migrate-findings">
          {preview.findings.map((finding) => (
            <li key={finding.message} className="flex flex-wrap gap-x-3">
              <Mono size="data" tone="amber">
                {FINDING_LABEL[finding.kind]}
              </Mono>
              <span className="text-ink-2">{finding.message}</span>
            </li>
          ))}
        </ul>
      )}
      {dirty ? (
        <Note>
          The migration reads the saved v{preview.base_version}. Save or discard your edits first.
        </Note>
      ) : null}
      <span className="flex flex-wrap gap-2">
        <Button variant="filled" className="uppercase" disabled={applying || dirty} onClick={onApply}>
          {applying ? 'Saving…' : `Save as v${preview.base_version + 1}`}
        </Button>
        <Button className="uppercase" disabled={applying} onClick={onCancel}>
          Cancel
        </Button>
      </span>
    </div>
  );
}
