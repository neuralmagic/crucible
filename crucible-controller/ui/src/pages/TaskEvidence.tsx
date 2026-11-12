import { useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { $api, fetchRunFile } from '../api/client';
import { CodeSurface } from '../editor/CodeSurface';
import { LogView } from '../logs/LogView';
import { cn, Mono, Spinner, Split, SplitHandle, SplitPane } from '../ui';
import { formatError } from '../api/errors';
import { sandboxedDocument } from './decisions';
import { RUN_FILE_CSP, inlineKind } from './runFileView';
import { evidenceItems, logBody, logPath, pendingLine, type EvidenceItem } from './taskEvidenceView';

const LOG_ID = 'log';
const EVIDENCE_PANES = ['list', 'content'];

const CONTENT_TEST_ID: Record<string, string> = {
  result: 'task-result',
  payload: 'task-payload',
  log: 'run-log',
};

/// Prose evidence: the task's own note, and the engine log.
function Prose({ body, testId }: { body: string; testId?: string }) {
  return (
    <pre
      data-testid={testId}
      className="m-0 h-full overflow-auto bg-paper px-2 py-1.5 font-mono text-data whitespace-pre-wrap text-ink-2"
    >
      {body}
    </pre>
  );
}

/// An HTML file a task captured, opened as the document it is: the payload's copy when it carried
/// one, the run's file store otherwise, sandboxed the way the Files tab sandboxes one.
function EvidenceDocument({ runId, item }: { runId: string; item: EvidenceItem }) {
  const fetched = useQuery({
    queryKey: ['task-evidence-file', runId, item.key],
    queryFn: () => fetchRunFile(runId, item.key ?? ''),
    enabled: item.body === null && item.key !== null,
  });
  const source = item.body ?? fetched.data ?? null;
  if (source === null) {
    if (fetched.isError) {
      return (
        <div className="px-2 py-1.5">
          <Mono tone="ink-3">{formatError(fetched.error)}</Mono>
        </div>
      );
    }
    return (
      <div className="px-2 py-1.5">
        <Spinner label="Opening document" />
      </div>
    );
  }
  return (
    <div className="min-h-0 flex-1">
      <iframe
        className="h-full w-full border-0 bg-surface"
        title={item.name}
        sandbox="allow-scripts"
        referrerPolicy="no-referrer"
        srcDoc={sandboxedDocument(source, 'text/html', RUN_FILE_CSP)}
      />
    </div>
  );
}

interface RowProps {
  item: EvidenceItem;
  selected: boolean;
  onSelect: () => void;
}

function Row({ item, selected, onSelect }: RowProps) {
  return (
    <button
      type="button"
      data-testid={item.kind === 'file' ? 'task-file' : undefined}
      aria-pressed={selected}
      onClick={onSelect}
      title={item.name}
      className={cn(
        'flex w-full cursor-pointer items-baseline justify-between gap-2 border-0 px-2 py-0.5 text-left font-mono text-micro tracking-label uppercase',
        selected ? 'bg-hi font-semibold text-ink' : 'bg-transparent text-ink-2 hover:text-ink'
      )}
    >
      <span className="min-w-0 truncate">{item.name}</span>
      {item.note === null ? null : <span className="flex-none text-ink-3">{item.note}</span>}
    </button>
  );
}

/// What one task of a run actually did: its note, the payload it emitted, the files it captured,
/// and the run's engine output, picked out of a list and read in the pane beside it. A task the run
/// is still working on says so instead of reading as a finished one with nothing to show.
///
/// Every body is React text, so a pack's own output can never be markup here.
export function TaskEvidence({ runId, task }: { runId: string; task: string }) {
  const [picked, setPicked] = useState<string | null>(null);
  const evidence = $api.useQuery(
    'get',
    '/api/runs/{run_id}/tasks/{task}/evidence',
    { params: { path: { run_id: runId, task } } },
    { retry: false }
  );
  // The run log is the panel's default selection when a task has no evidence of its own yet —
  // mid-run, usually — so it must read in that state too, not only once picked by hand.
  const log = $api.useQuery(
    'get',
    '/api/runs/{run_id}/log',
    { params: { path: { run_id: runId } } },
    {
      retry: false,
      enabled:
        picked === LOG_ID ||
        (picked === null && evidence.data !== undefined && evidenceItems(evidence.data).length === 0),
    }
  );

  if (evidence.data === undefined) return null;
  const found = evidence.data;
  const pending = pendingLine(found);

  const items: EvidenceItem[] = [
    ...evidenceItems(found),
    {
      id: LOG_ID,
      kind: 'log',
      name: 'run log',
      note: null,
      path: log.data === undefined ? null : logPath(log.data),
      body: log.data === undefined ? 'reading the run log…' : logBody(log.data),
      key: null,
    },
  ];
  const selected = items.find((item) => item.id === picked) ?? items[0];

  return (
    <>
      {pending !== null && (
        <p data-testid="task-pending" className="mt-3 mb-0 font-mono text-data text-ink-3">
          {pending}
        </p>
      )}
      <Split
        id="crucible.evidence"
        panelIds={EVIDENCE_PANES}
        orientation="vertical"
        className="mt-3 h-[24rem] border border-rule-hard"
      >
        <SplitPane id="list" defaultSize="30%" minSize="3rem" className="overflow-y-auto bg-surface">
          <p className="m-0 border-b border-rule bg-sunk px-2 py-0.5 font-mono text-micro tracking-label text-ink-3 uppercase">
            Evidence
          </p>
          {items.map((item) => (
            <Row
              key={item.id}
              item={item}
              selected={item.id === selected.id}
              onSelect={() => {
                setPicked(item.id);
              }}
            />
          ))}
        </SplitPane>
        <SplitHandle orientation="vertical" label="Resize the evidence content" />
        <SplitPane id="content" minSize="3rem">
          <div data-testid="task-evidence-content" className="flex min-h-0 flex-1 flex-col">
            {selected.kind === 'file' && inlineKind(selected.name) === 'html' ? (
              <EvidenceDocument runId={runId} item={selected} />
            ) : selected.body === null ? (
              <p className="m-0 px-2 py-1.5 font-mono text-data text-ink-3">
                {`${selected.name} was captured but not carried inline; it opens from the run's Files tab.`}
              </p>
            ) : selected.path === null ? (
              <Prose body={selected.body} testId={CONTENT_TEST_ID[selected.kind]} />
            ) : selected.kind === 'log' ? (
              <LogView text={selected.body} testId={CONTENT_TEST_ID[selected.kind]} />
            ) : (
              <CodeSurface
                path={selected.path}
                value={selected.body}
                readOnly
                label={`${selected.path} contents`}
                testId={CONTENT_TEST_ID[selected.kind]}
              />
            )}
          </div>
        </SplitPane>
      </Split>
    </>
  );
}
