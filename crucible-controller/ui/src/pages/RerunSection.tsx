import { useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import type { components } from '../api/schema';
import { Button, Mono, Section, SectionBody, SectionHeader, Status } from '../ui';
import type { StatusTone } from '../ui';
import { launchPath, rerunLaunchBody, rerunRows, withTask } from './launchView';
import type { RerunState } from './launchView';
import { formatCost } from './runReport';

type LaunchDetail = components['schemas']['PlaybookLaunchDetailDto'];

const TONE: Record<RerunState, StatusTone> = {
  reused: 'green',
  reruns: 'blue',
  changed: 'amber',
};

/// Rerun this launch's finished run from a task: what is reused and what runs again, against the
/// pack the rerun would run.
export function RerunSection({ detail, runId }: { detail: LaunchDetail; runId: string }) {
  const navigate = useNavigate();
  const [from, setFrom] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const preview = $api.useQuery('get', '/api/runs/{run_id}/rerun', {
    params: { path: { run_id: runId }, query: { from: from.join(',') } },
  });
  const draftLaunch = $api.useMutation('post', '/api/playbook-drafts/{id}/launch');
  const packLaunch = $api.useMutation('post', '/api/playbooks/{id}/launch');
  const { launch } = detail;
  const draft = launch.draft_version !== null && launch.draft_version !== undefined;

  if (preview.data === undefined) return null;
  const rows = rerunRows(preview.data);
  const blocked = preview.data.changed.length > 0;
  const pending = draftLaunch.isPending || packLaunch.isPending;

  const handleRerun = async () => {
    setError(null);
    const request = {
      params: { path: { id: launch.playbook } },
      body: rerunLaunchBody(launch, runId, from),
    };
    try {
      const ack = draft
        ? await draftLaunch.mutateAsync(request)
        : await packLaunch.mutateAsync(request);
      void navigate(launchPath(ack.key));
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  return (
    <Section>
      <SectionHeader
        title="Rerun"
        note={`${formatCost(preview.data.reused_usd)} reused`}
        actions={
          <Button
            variant="filled"
            className="uppercase"
            disabled={blocked || pending}
            onClick={() => {
              void handleRerun();
            }}
          >
            {pending ? 'Rerunning…' : `Rerun ${preview.data.rerun.length}`}
          </Button>
        }
      />
      <SectionBody className="grid gap-2">
        <ul data-testid="rerun-tasks" className="m-0 list-none p-0">
          {rows.map((row) => (
            <li
              key={row.task}
              data-state={row.state}
              className="flex items-center gap-3 border-b border-rule py-1.5 last:border-b-0"
            >
              <Mono size="data">{row.task}</Mono>
              <Status status={row.state} tone={TONE[row.state]} pulse={false} />
              <span className="flex-1" />
              {row.costUsd !== null && (
                <Mono size="data" tone="ink-3">
                  {formatCost(row.costUsd)}
                </Mono>
              )}
              {row.state !== 'reruns' && (
                <Button
                  className="uppercase"
                  onClick={() => {
                    setFrom(withTask(from, row.task));
                  }}
                >
                  {row.state === 'changed' ? 'Include' : 'Rerun from'}
                </Button>
              )}
            </li>
          ))}
        </ul>
        {error !== null && (
          <p data-testid="rerun-error" className="m-0 whitespace-pre-wrap font-mono text-data text-red">
            {error}
          </p>
        )}
      </SectionBody>
    </Section>
  );
}
