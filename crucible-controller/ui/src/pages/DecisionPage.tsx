import { useQueryClient } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import { Link, useParams } from 'react-router-dom';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import type { components } from '../api/schema';
import { useLiveEvents } from '../api/useLiveEvents';
import {
  Button,
  cn,
  DetailHeader,
  Mono,
  QueryState,
  Section,
  SectionBody,
  SectionHeader,
  Spec,
  Status,
  type StatusTone,
} from '../ui';
import { usd } from '../budget';
import { MarkdownView } from './MarkdownView';
import {
  choose,
  decodeText,
  fileView,
  labelTone,
  type LabelTone,
  parseCsv,
  remaining,
  sandboxedDocument,
} from './decisions';

type DecisionDto = components['schemas']['DecisionDto'];
type DecisionFileDto = components['schemas']['DecisionFileDto'];

const TONE: Record<string, StatusTone> = {
  open: 'amber',
  answered: 'green',
  expired: 'grey',
  withdrawn: 'grey',
};

function duration(secs: number): string {
  if (secs < 3600) return `${String(Math.round(secs / 60))}m`;
  return `${(secs / 3600).toFixed(1)}h`;
}

export function DecisionPage() {
  useLiveEvents();
  const { id = '' } = useParams();
  const decision = $api.useQuery('get', '/api/decisions/{id}', { params: { path: { id } } });
  return (
    <QueryState query={decision} noun="decision request">
      {decision.data ? <Decision d={decision.data} /> : null}
    </QueryState>
  );
}

function useNow(): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = setInterval(() => {
      setNow(Date.now());
    }, 1000);
    return () => {
      clearInterval(timer);
    };
  }, []);
  return now;
}

function Decision({ d }: { d: DecisionDto }) {
  const now = useNow();
  const launch = d.launch_key ? (
    <Link className="text-blue" to={`/playbook-runs/${encodeURIComponent(d.launch_key)}`}>
      {d.launch_key}
    </Link>
  ) : null;
  return (
    <>
      <DetailHeader
        title={d.task}
        badge={<Status status={d.state} tone={TONE[d.state] ?? 'grey'} />}
        meta={
          <span className="flex flex-wrap gap-3">
            {launch}
            <Mono size="data" tone="ink-3">
              {d.run_id}
            </Mono>
            {d.state === 'open' ? (
              <Mono size="data" tone="amber">
                expires {remaining(d.expires_at, now)}
              </Mono>
            ) : null}
          </span>
        }
        aside={
          <Spec
            items={[
              { label: 'Spent', value: usd(d.spent_usd), note: `of ${usd(d.max_cost_usd)}` },
              {
                label: 'Elapsed',
                value: duration(d.elapsed_secs),
                note: d.max_time_secs ? `of ${duration(d.max_time_secs)}` : undefined,
              },
            ]}
          />
        }
      />
      {d.review ? (
        <Section>
          <SectionHeader title="Review" />
          <SectionBody>
            <MarkdownView markdown={d.review} />
          </SectionBody>
        </Section>
      ) : null}
      <Answer d={d} />
      {d.gated.length > 0 ? (
        <Section>
          <SectionHeader title="Each answer starts" />
          <SectionBody>
            <ul className="m-0 list-none p-0" data-testid="decision-gated">
              {d.gated.map((g) => (
                <li key={`${g.name}-${g.question}`} className="flex gap-3 py-0.5">
                  <Mono size="data" tone="ink-3">
                    {g.question} = {g.labels.join(' | ')}
                  </Mono>
                  <Mono size="data">{g.name}</Mono>
                  <Mono size="data" tone="ink-3">
                    {g.kind}
                  </Mono>
                </li>
              ))}
            </ul>
          </SectionBody>
        </Section>
      ) : null}
      {d.inputs.map((input) => (
        <Section key={input.task}>
          <SectionHeader
            title={input.task}
            note={<Status status={input.status} tone={input.status === 'pass' ? 'green' : 'red'} />}
          />
          <SectionBody>
            {input.output ? (
              <pre className="m-0 overflow-x-auto font-mono text-data-lg text-ink" data-testid="decision-output">
                {JSON.stringify(input.output, null, 2)}
              </pre>
            ) : null}
            {input.files.map((file) => (
              <EvidenceFile key={file.path} file={file} />
            ))}
          </SectionBody>
        </Section>
      ))}
    </>
  );
}

function Answer({ d }: { d: DecisionDto }) {
  const queryClient = useQueryClient();
  const [labels, setLabels] = useState<Record<string, string[]>>({});
  const [note, setNote] = useState('');
  const mutation = $api.useMutation('post', '/api/decisions/{id}/answer');

  if (d.answer) {
    return (
      <Section>
        <SectionHeader title="Answer" />
        <SectionBody>
          <div className="flex flex-wrap gap-3" data-testid="decision-answer">
            {Object.entries(d.answer.labels).map(([q, values]) => (
              <span key={q} className="flex flex-wrap items-baseline gap-2">
                <Mono size="data" tone="ink-3">
                  {q}
                </Mono>
                {values.map((value) => (
                  <span
                    key={value}
                    className={cn(CHIP, CHOSEN[labelTone(value)], kindOf(d, q) === 'pick' && 'normal-case')}
                  >
                    ✓ {value}
                  </span>
                ))}
              </span>
            ))}
            <Mono size="data" tone="ink-3">
              {d.answer.decided_by} · {d.answer.decided_at}
            </Mono>
          </div>
          {d.answer.note ? <p className="mb-0 text-ink-2">{d.answer.note}</p> : null}
        </SectionBody>
      </Section>
    );
  }
  if (!d.can_answer) return null;

  const complete = d.questions.every((q) => (labels[q.id] ?? []).length > 0);
  return (
    <Section>
      <SectionHeader title="Answer" />
      <SectionBody>
        {d.questions.map((q) => (
          <fieldset key={q.id} className="m-0 mb-3 border-0 p-0" data-kind={q.kind} data-multiple={q.multiple}>
            <legend className="mb-1.5 flex items-baseline gap-2 text-ink">
              {q.instructions}
              {q.multiple ? (
                <Mono size="data" tone="ink-3">
                  any
                </Mono>
              ) : null}
            </legend>
            <div className="flex flex-wrap gap-2">
              {q.options.map((option) => (
                <LabelChoice
                  key={option}
                  label={option}
                  literal={q.kind === 'pick'}
                  chosen={(labels[q.id] ?? []).includes(option)}
                  onChoose={() => {
                    setLabels((current) => ({
                      ...current,
                      [q.id]: choose(current[q.id] ?? [], option, q.multiple),
                    }));
                  }}
                />
              ))}
            </div>
          </fieldset>
        ))}
        <textarea
          aria-label="Note"
          className="mb-3 w-full border border-rule-hard bg-surface px-2 py-1.5 text-ink"
          rows={2}
          value={note}
          onChange={(e) => {
            setNote(e.target.value);
          }}
        />
        <div className="flex items-center gap-3">
          <Button
            variant="filled"
            disabled={!complete || mutation.isPending}
            onClick={() => {
              mutation.mutate(
                {
                  params: { path: { id: d.id } },
                  body: { labels, evidence_digest: d.evidence_digest, note: note.trim() || null },
                },
                {
                  onSuccess: () => {
                    void queryClient.invalidateQueries({ queryKey: ['get', '/api/decisions'] });
                    void queryClient.invalidateQueries({ queryKey: ['get', '/api/decisions/{id}'] });
                  },
                },
              );
            }}
          >
            {mutation.isPending ? 'Submitting…' : 'Submit'}
          </Button>
          {mutation.isError ? (
            <span role="alert" className="text-red">
              {formatError(mutation.error)}
            </span>
          ) : null}
        </div>
      </SectionBody>
    </Section>
  );
}

const CHIP =
  'inline-flex min-w-24 items-center justify-center gap-1.5 border px-3.5 py-1.5 font-mono text-data font-semibold uppercase tracking-action';

/// An unchosen label: outlined in its tone.
const OFFERED: Record<LabelTone, string> = {
  go: 'border-green text-green hover:bg-green/10',
  stop: 'border-red text-red hover:bg-red/10',
  neutral: 'border-rule-hard text-ink-2 hover:bg-hi',
};

/// The chosen label: filled in its tone.
const CHOSEN: Record<LabelTone, string> = {
  go: 'border-green bg-green text-surface',
  stop: 'border-red bg-red text-surface',
  neutral: 'border-ink bg-ink text-surface',
};

function kindOf(d: DecisionDto, question: string): string | undefined {
  return d.questions.find((q) => q.id === question)?.kind;
}

function LabelChoice({
  label,
  literal,
  chosen,
  onChoose,
}: {
  label: string;
  literal: boolean;
  chosen: boolean;
  onChoose: () => void;
}) {
  const tone = labelTone(label);
  return (
    <button
      type="button"
      aria-pressed={chosen}
      data-tone={tone}
      className={cn(CHIP, 'cursor-pointer', literal && 'normal-case', chosen ? CHOSEN[tone] : OFFERED[tone])}
      onClick={onChoose}
    >
      {chosen ? '✓ ' : ''}
      {label}
    </button>
  );
}

function EvidenceFile({ file }: { file: DecisionFileDto }) {
  const view = fileView(file.media_type);
  const href = `data:${file.media_type};base64,${file.base64}`;
  return (
    <figure className="m-0 mt-3" data-testid="decision-file" data-view={view}>
      <figcaption className="mb-1 flex gap-3">
        <Mono size="data">{file.path}</Mono>
        <a className="font-mono text-data text-blue" href={href} download={file.path.split('/').pop()}>
          download
        </a>
      </figcaption>
      <FileBody file={file} view={view} href={href} />
    </figure>
  );
}

function FileBody({
  file,
  view,
  href,
}: {
  file: DecisionFileDto;
  view: ReturnType<typeof fileView>;
  href: string;
}) {
  switch (view) {
    case 'image':
      return <img className="max-w-full border border-rule" src={href} alt={file.path} />;
    case 'sandboxed':
      return (
        <iframe
          title={file.path}
          className="h-[480px] w-full border border-rule bg-white"
          sandbox="allow-scripts"
          referrerPolicy="no-referrer"
          srcDoc={sandboxedDocument(decodeText(file.base64), file.media_type)}
        />
      );
    case 'markdown':
      return <MarkdownView markdown={decodeText(file.base64)} />;
    case 'csv': {
      const [head = [], ...rows] = parseCsv(decodeText(file.base64));
      return (
        <table className="border-collapse font-mono text-data-lg">
          <thead>
            <tr>
              {head.map((h, i) => (
                <th key={i} className="border border-rule px-2 py-1 text-left text-ink-2">
                  {h}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.map((row, r) => (
              <tr key={r}>
                {row.map((cell, c) => (
                  <td key={c} className="border border-rule px-2 py-1">
                    {cell}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      );
    }
    case 'json':
    case 'text':
      return (
        <pre className="m-0 max-h-[480px] overflow-auto font-mono text-data-lg">{decodeText(file.base64)}</pre>
      );
    case 'download':
      return null;
  }
}
