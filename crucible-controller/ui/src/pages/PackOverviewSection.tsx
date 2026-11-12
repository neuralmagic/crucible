import { useState } from 'react';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import {
  cn,
  Mono,
  Section,
  SectionBody,
  SectionHeader,
  Spinner,
  Split,
  SplitHandle,
  SplitPane,
} from '../ui';

const PACK_PANES = ['list', 'content'];

/// The pack a launch froze, as its run page shows it: the intent the operator handed the drafting
/// turn, then every file that version holds. The task panels below show what the run did with the
/// pack; this is the one place that shows what the pack is — which model, which repo, which
/// benchmark config — before and while it runs, not only in the evidence it leaves.
export function PackOverviewSection({ draftId, version }: { draftId: string; version: number }) {
  const pack = $api.useQuery('get', '/api/playbook-drafts/{id}/files', {
    params: { path: { id: draftId }, query: { version } },
  });

  if (pack.isPending) {
    return (
      <Section>
        <SectionHeader title="Pack" note={`draft v${version}`} />
        <SectionBody>
          <Spinner label="Loading the pack" />
        </SectionBody>
      </Section>
    );
  }
  if (pack.isError) {
    return (
      <Section>
        <SectionHeader title="Pack" note={`draft v${version}`} />
        <SectionBody>
          <Mono tone="ink-3">The pack could not be loaded: {formatError(pack.error)}</Mono>
        </SectionBody>
      </Section>
    );
  }

  const { intent, files } = pack.data;
  const names = Object.keys(files);

  return (
    <Section>
      <SectionHeader
        title="Pack"
        note={`draft v${version} · frozen at launch · ${names.length} files`}
      />
      <SectionBody className="grid gap-3">
        {intent !== null && intent !== undefined && intent.trim() !== '' && (
          <div data-testid="pack-intent">
            <Mono size="micro" tone="ink-3" uppercase className="tracking-section">
              Intent
            </Mono>
            <p
              className="mt-1 mb-0 max-h-40 overflow-y-auto whitespace-pre-wrap border border-rule bg-sunk px-2 py-1.5 font-mono text-data text-ink-2"
              title="The intent the drafting turn was handed"
            >
              {intent}
            </p>
          </div>
        )}
        <PackFiles files={files} />
      </SectionBody>
    </Section>
  );
}

/// The version's file map, master-detail like the run's captured files: pick a file on the left,
/// read it on the right. Pack files are the pack's own source, so they read as code, not as
/// rendered output.
function PackFiles({ files }: { files: Record<string, string> }) {
  const names = Object.keys(files);
  const [selected, setSelected] = useState(() => initialPackFile(names));
  const current = names.includes(selected) ? selected : (names[0] ?? null);

  if (names.length === 0) {
    return <Mono tone="ink-3">This version holds no files.</Mono>;
  }

  return (
    <Split
      id="crucible.pack-files"
      panelIds={PACK_PANES}
      orientation="horizontal"
      className="h-[28rem] border border-rule-hard"
    >
      <SplitPane id="list" defaultSize="24%" minSize="3rem" className="overflow-y-auto bg-surface">
        {names.map((name) => (
          <button
            key={name}
            type="button"
            aria-pressed={name === current}
            onClick={() => {
              setSelected(name);
            }}
            title={name}
            className={cn(
              'block w-full cursor-pointer overflow-hidden truncate border-0 bg-transparent px-2 py-0.5 text-left font-mono text-micro tracking-label',
              name === current ? 'bg-hi font-semibold text-ink' : 'text-ink-2 hover:text-ink',
            )}
          >
            {name}
          </button>
        ))}
      </SplitPane>
      <SplitHandle orientation="horizontal" label="Resize the pack file content" />
      <SplitPane id="content" minSize="3rem">
        <div className="flex h-full min-h-0 flex-col">
          <p className="m-0 border-b border-rule bg-sunk px-2 py-0.5 font-mono text-micro tracking-label text-ink-2">
            {current}
          </p>
          <pre className="m-0 min-h-0 flex-1 overflow-auto px-2 py-1.5 font-mono text-data text-ink">
            {current !== null ? files[current] : ''}
          </pre>
        </div>
      </SplitPane>
    </Split>
  );
}

/// The file the pane opens on: the declaration a reader scans first when there is one.
function initialPackFile(names: string[]): string {
  for (const name of ['experiment.json', 'crucible.toml']) {
    if (names.includes(name)) return name;
  }
  return names[0] ?? '';
}
