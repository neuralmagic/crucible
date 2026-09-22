import { useState } from 'react';
import { $api } from '../api/client';
import { CodeSurface } from '../editor/CodeSurface';
import { cn, Split, SplitHandle, SplitPane } from '../ui';
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
  const log = $api.useQuery(
    'get',
    '/api/runs/{run_id}/log',
    { params: { path: { run_id: runId } } },
    { retry: false, enabled: picked === LOG_ID }
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
            {selected.body === null ? (
              <p className="m-0 px-2 py-1.5 font-mono text-data text-ink-3">
                {`${selected.name} was captured but not carried inline.`}
              </p>
            ) : selected.path === null ? (
              <Prose body={selected.body} testId={CONTENT_TEST_ID[selected.kind]} />
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
