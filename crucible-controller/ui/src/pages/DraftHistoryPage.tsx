import { useMemo } from 'react';
import { Link, useParams, useSearchParams } from 'react-router-dom';
import { $api } from '../api/client';
import type { components } from '../api/schema';
import {
  Breadcrumb,
  Button,
  cn,
  Empty,
  FoldSection,
  formatStamp,
  Mono,
  PageHeader,
  QueryState,
  Section,
  SectionBody,
  SectionHeader,
  Status,
  statusTone,
} from '../ui';
import { useMediaQuery } from '../useMediaQuery';
import { launchPath, launchStatusColor, launchStatusLabel } from './launchView';
import {
  CHANGE_LETTER,
  halvesOf,
  launchesByVersion,
  neighborsOf,
  parseUnified,
  placementOf,
  rangeLabel,
  rangeOf,
  rangeSearch,
  singleSave,
  type DiffLineKind,
  type FileChange,
  type FileDiff,
  type HistoryEntry,
  type Placement,
  type Range,
} from './draftHistory';

type PlaybookRun = components['schemas']['PlaybookRunDto'];

const WIDE = '(min-width: 1200px)';
const LARGE_DIFF_ROWS = 1500;
const NO_HISTORY: readonly HistoryEntry[] = [];
const NO_RUNS: readonly PlaybookRun[] = [];

const CHANGE_TONE: Record<FileChange, string> = {
  added: 'text-green',
  removed: 'text-red',
  modified: 'text-amber',
};

const PLACEMENT_EDGE: Record<Placement, string> = {
  to: 'border-l-ink bg-hi',
  from: 'border-l-ink bg-hi',
  inside: 'border-l-rule-hard',
  outside: 'border-l-transparent',
};

const ROW: Record<DiffLineKind, string> = {
  hunk: 'bg-sunk text-ink-3',
  add: 'bg-green/14 text-ink',
  del: 'bg-red/14 text-ink',
  context: 'text-ink-2',
  note: 'text-ink-3 italic',
};

const SIGN: Record<DiffLineKind, string> = {
  hunk: '',
  add: '+',
  del: '-',
  context: '',
  note: '',
};

export function DraftHistoryPage() {
  const { id = '' } = useParams<{ id: string }>();
  const [search] = useSearchParams();
  const wide = useMediaQuery(WIDE);
  const history = $api.useQuery('get', '/api/playbook-drafts/{id}/history', {
    params: { path: { id } },
  });
  const runs = $api.useQuery('get', '/api/playbook-runs', {
    params: { query: { playbook: id } },
  });
  const entries = history.data ?? NO_HISTORY;
  const range = rangeOf(search, entries);
  const launches = useMemo(() => launchesByVersion(runs.data ?? NO_RUNS), [runs.data]);
  const studio = `/playbooks/drafts/${encodeURIComponent(id)}`;

  return (
    <div className="flex h-full min-h-0 flex-col">
      <Breadcrumb
        items={[
          { label: 'Playbooks', to: '/playbooks' },
          { label: 'Drafts', to: '/playbooks/drafts' },
          { label: id, to: studio },
          { label: 'History' },
        ]}
      />
      <PageHeader
        eyebrow="Draft history"
        title={id}
        actions={
          <Button className="uppercase" render={<Link to={studio} />}>
            Open studio
          </Button>
        }
      />
      <QueryState query={history} noun="history">
        {range === null ? (
          <Empty title="No saves" />
        ) : (
          <div className={cn('flex min-h-0 flex-1', wide ? 'flex-row' : 'flex-col')}>
            <div
              className={cn(
                'min-h-0 overflow-y-auto border-rule-hard',
                wide ? 'w-[26rem] shrink-0 border-r' : 'max-h-[40vh] border-b',
              )}
            >
              <Changelog entries={entries} range={range} launches={launches} />
            </div>
            <div className="min-h-0 min-w-0 flex-1 overflow-y-auto">
              <RangeDiff id={id} entries={entries} range={range} />
            </div>
          </div>
        )}
      </QueryState>
    </div>
  );
}

function RangeLink({
  range,
  children,
  testId,
}: {
  range: Range | null;
  children: string;
  testId?: string;
}) {
  if (range === null) {
    return (
      <Button className="uppercase" disabled data-testid={testId}>
        {children}
      </Button>
    );
  }
  return (
    <Button className="uppercase" data-testid={testId} render={<Link to={rangeSearch(range)} replace />}>
      {children}
    </Button>
  );
}

function Changelog({
  entries,
  range,
  launches,
}: {
  entries: readonly HistoryEntry[];
  range: Range;
  launches: ReadonlyMap<number, readonly PlaybookRun[]>;
}) {
  return (
    <Section className="border-b-0">
      <SectionHeader title="Saves" />
      <ol data-testid="draft-history-saves">
        {entries.map((entry) => (
          <SaveRow
            key={entry.version}
            entry={entry}
            entries={entries}
            range={range}
            launches={launches.get(entry.version) ?? NO_RUNS}
          />
        ))}
      </ol>
    </Section>
  );
}

function SaveRow({
  entry,
  entries,
  range,
  launches,
}: {
  entry: HistoryEntry;
  entries: readonly HistoryEntry[];
  range: Range;
  launches: readonly PlaybookRun[];
}) {
  const placement = placementOf(range, entry.version);
  const compiles = entry.schema_digest !== null && entry.schema_digest !== undefined;
  return (
    <li
      data-testid={`draft-history-save-${entry.version}`}
      data-placement={placement}
      className={cn('border-b border-l-2 border-b-rule bg-surface px-4 py-2.5', PLACEMENT_EDGE[placement])}
    >
      <div className="flex items-center gap-2.5">
        <Link
          to={rangeSearch(singleSave(entries, entry.version))}
          replace
          className="font-mono text-data-lg font-semibold text-ink hover:underline"
        >
          {`v${entry.version}`}
        </Link>
        <Status
          status={compiles ? 'compiles' : `${entry.diagnostics} diagnostics`}
          tone={compiles ? 'green' : 'red'}
        />
        <span className="ml-auto flex items-center gap-1">
          <Button
            className="uppercase"
            selected={placement === 'from'}
            render={<Link to={rangeSearch({ from: entry.version, to: range.to })} replace />}
          >
            From
          </Button>
          <Button
            className="uppercase"
            selected={placement === 'to'}
            render={
              <Link
                to={rangeSearch(
                  range.from === null
                    ? singleSave(entries, entry.version)
                    : { from: range.from, to: entry.version },
                )}
                replace
              />
            }
          >
            To
          </Button>
        </span>
      </div>
      <Mono size="label" tone="ink-3" className="block">
        {`${entry.created_by ?? 'unknown'} · ${formatStamp(entry.created_at)}`}
      </Mono>
      <ChangedFiles changes={entry.changes} />
      {launches.length > 0 && (
        <div className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1">
          {launches.map((run) => (
            <Link key={run.key} to={launchPath(run.key)} title={run.key} className="hover:underline">
              <Status
                status={launchStatusLabel(run.status)}
                tone={statusTone(launchStatusColor(run.status))}
              />
            </Link>
          ))}
        </div>
      )}
    </li>
  );
}

function ChangedFiles({ changes }: { changes: HistoryEntry['changes'] }) {
  if (changes === null || changes === undefined) {
    return (
      <Mono size="label" tone="ink-3" className="mt-1 block">
        Changes unknown
      </Mono>
    );
  }
  if (changes.length === 0) {
    return (
      <Mono size="label" tone="ink-3" className="mt-1 block">
        No file changes
      </Mono>
    );
  }
  return (
    <ul className="mt-1">
      {changes.map((file) => (
        <li key={file.path} className="flex gap-2 font-mono text-label">
          <span className={cn('w-3 shrink-0 font-semibold', CHANGE_TONE[file.change])}>
            {CHANGE_LETTER[file.change]}
          </span>
          <span className="break-all text-ink-2">{file.path}</span>
        </li>
      ))}
    </ul>
  );
}

function RangeDiff({
  id,
  entries,
  range,
}: {
  id: string;
  entries: readonly HistoryEntry[];
  range: Range;
}) {
  const diff = $api.useQuery('get', '/api/playbook-drafts/{id}/diff', {
    params: {
      path: { id },
      query: range.from === null ? { to: range.to } : { from: range.from, to: range.to },
    },
  });
  const halves = halvesOf(entries, range);
  const neighbors = neighborsOf(entries, range);
  const files = diff.data?.files;
  const additions = files?.reduce((sum, file) => sum + file.additions, 0) ?? 0;
  const deletions = files?.reduce((sum, file) => sum + file.deletions, 0) ?? 0;

  return (
    <>
      <SectionHeader
        title={<span data-testid="draft-history-range">{rangeLabel(range)}</span>}
        note={files === undefined ? undefined : `+${additions} −${deletions}`}
        actions={
          <>
            {halves !== null && (
              <span data-testid="draft-history-bisect" className="flex items-center gap-2">
                <RangeLink range={halves.older} testId="draft-history-older-half">
                  {rangeLabel(halves.older)}
                </RangeLink>
                <RangeLink range={halves.newer} testId="draft-history-newer-half">
                  {rangeLabel(halves.newer)}
                </RangeLink>
              </span>
            )}
            <RangeLink range={neighbors.older} testId="draft-history-older">
              Older save
            </RangeLink>
            <RangeLink range={neighbors.newer} testId="draft-history-newer">
              Newer save
            </RangeLink>
          </>
        }
        className="sticky top-0 z-10 flex-wrap"
      />
      <QueryState query={diff} noun="diff">
        {files === undefined || files.length === 0 ? (
          <Empty title="No changes" />
        ) : (
          <div data-testid="draft-history-diff">
            {files.map((file) => (
              <FileDiffSection key={`${rangeSearch(range)}:${file.path}`} file={file} />
            ))}
          </div>
        )}
      </QueryState>
    </>
  );
}

function FileDiffSection({ file }: { file: FileDiff }) {
  const rows = useMemo(
    () => (file.unified === null || file.unified === undefined ? null : parseUnified(file.unified)),
    [file.unified],
  );
  return (
    <FoldSection
      defaultOpen={rows === null || rows.length <= LARGE_DIFF_ROWS}
      title={
        <span className="flex items-center gap-2 normal-case tracking-normal">
          <span className={cn('font-semibold', CHANGE_TONE[file.change])}>
            {CHANGE_LETTER[file.change]}
          </span>
          <span className="break-all text-ink">{file.path}</span>
        </span>
      }
      note={rows === null ? 'not text' : `+${file.additions} −${file.deletions}`}
    >
      {rows === null ? (
        <SectionBody>
          <Mono size="label" tone="ink-3">
            Not text
          </Mono>
        </SectionBody>
      ) : (
        <div className="overflow-x-auto bg-paper" data-testid={`draft-history-file-${file.path}`}>
          <div className="min-w-max font-mono text-data leading-5">
            {rows.map((row, index) => (
              <div key={index} data-kind={row.kind} className={cn('flex whitespace-pre', ROW[row.kind])}>
                <span className="w-12 shrink-0 px-2 text-right text-ink-3 select-none">
                  {row.oldLine ?? ''}
                </span>
                <span className="w-12 shrink-0 px-2 text-right text-ink-3 select-none">
                  {row.newLine ?? ''}
                </span>
                <span className="w-5 shrink-0 text-center select-none">{SIGN[row.kind]}</span>
                <span className="pr-4">{row.text}</span>
              </div>
            ))}
          </div>
        </div>
      )}
    </FoldSection>
  );
}
