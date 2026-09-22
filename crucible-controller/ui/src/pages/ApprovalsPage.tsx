import { useState } from 'react';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import type { components } from '../api/schema';
import { useLiveEvents } from '../api/useLiveEvents';
import {
  Button,
  cn,
  createDataColumnHelper,
  DataTable,
  Empty,
  Identifier,
  Mono,
  PageHeader,
  PrLink,
  QueryState,
  Section,
  SectionHeader,
  Spinner,
  Status,
  statusTone,
  useDataTable,
} from '../ui';
import type { ControlEvidence, RoundRecord } from './approvalEvidence';
import {
  adversaryAttacks,
  adversaryPassed,
  contractStderrTail,
  lastAdversaryRound,
  outcomeSummary,
  roundOutcomeTone,
  selftestEvidence,
} from './approvalEvidence';

type AwaitingApprovalDto = components['schemas']['AwaitingApprovalDto'];
type KeptPrDto = components['schemas']['KeptPrDto'];
type PendingImportDto = components['schemas']['PendingImportDto'];

const packHelper = createDataColumnHelper<AwaitingApprovalDto>();

/// Abbreviate a `sha256:<64hex>` exposure digest for the queue; the full digest rides the tooltip,
/// and the block it digests is on the issue's own page.
function shortDigest(digest: string): string {
  const hex = digest.startsWith('sha256:') ? digest.slice('sha256:'.length) : digest;
  return `sha256:${hex.slice(0, 8)}…`;
}

const packColumns = packHelper.columns([
  packHelper.accessor('key', {
    header: 'Issue',
    enableSorting: false,
    meta: { pad: 'tight', shrink: true },
    cell: ({ getValue }) => (
      <Identifier to={`/issues/${encodeURIComponent(getValue())}`} title={getValue()}>
        {getValue()}
      </Identifier>
    ),
  }),
  packHelper.accessor('repo', {
    header: 'Repo',
    enableSorting: false,
    meta: { className: 'font-mono text-data text-ink-2' },
    cell: ({ getValue }) => getValue(),
  }),
  packHelper.accessor('scope_id', {
    header: 'Scope',
    enableSorting: false,
    meta: { align: 'end', shrink: true, className: 'font-mono text-data' },
    cell: ({ getValue }) => getValue(),
  }),
  packHelper.accessor('approval_pr', {
    header: 'Approval PR',
    enableSorting: false,
    meta: { shrink: true },
    cell: ({ getValue }) => <PrLink url={getValue()} />,
  }),
  packHelper.accessor('exposure_digest', {
    header: 'Exposure',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    cell: ({ getValue }) => {
      const digest = getValue() ?? null;
      return digest === null ? (
        <Mono size="micro" weight="bold" uppercase tone="amber" className="border border-amber px-1">
          undeclared
        </Mono>
      ) : (
        <span title={digest}>{shortDigest(digest)}</span>
      );
    },
  }),
  packHelper.accessor('stale', {
    header: 'Flags',
    enableSorting: false,
    meta: { shrink: true },
    cell: ({ getValue }) =>
      getValue() ? (
        <Mono size="micro" weight="bold" uppercase tone="amber" className="border border-amber px-1">
          stale
        </Mono>
      ) : (
        <Mono tone="ink-3">—</Mono>
      ),
  }),
]);

const importHelper = createDataColumnHelper<PendingImportDto>();

const importColumns = importHelper.columns([
  importHelper.accessor('id', {
    header: 'Import',
    enableSorting: false,
    meta: { pad: 'tight', shrink: true },
    cell: ({ getValue }) => (
      <Identifier to={`/playbooks/import/${getValue()}`} title={getValue()}>
        {getValue().slice(0, 8)}
      </Identifier>
    ),
  }),
  importHelper.accessor('repo', {
    header: 'Repo',
    enableSorting: false,
    meta: { className: 'font-mono text-data text-ink-2' },
    cell: ({ row, getValue }) =>
      row.original.path === '' ? getValue() : `${getValue()} · ${row.original.path}`,
  }),
  importHelper.accessor('rev', {
    header: 'Pinned at',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    cell: ({ getValue }) => getValue().slice(0, 12),
  }),
  importHelper.accessor('proposed_by', {
    header: 'Proposed by',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data' },
    cell: ({ getValue }) => getValue() ?? '—',
  }),
  importHelper.accessor('compiles', {
    header: 'Engine',
    enableSorting: false,
    meta: { shrink: true },
    cell: ({ row, getValue }) =>
      getValue() ? (
        <Status status="compiles" tone={statusTone('green')} />
      ) : (
        <Mono size="micro" weight="bold" uppercase tone="red" className="border border-red px-1">
          {`${String(row.original.diagnostics)} diagnostics`}
        </Mono>
      ),
  }),
]);

const prHelper = createDataColumnHelper<KeptPrDto>();

const prColumns = prHelper.columns([
  prHelper.accessor('issue', {
    header: 'Issue',
    enableSorting: false,
    meta: { pad: 'tight', shrink: true },
    cell: ({ getValue }) => (
      <Identifier to={`/issues/${encodeURIComponent(getValue())}`} title={getValue()}>
        {getValue()}
      </Identifier>
    ),
  }),
  prHelper.accessor('pr_url', {
    header: 'PR',
    enableSorting: false,
    meta: { wrap: true },
    cell: ({ getValue }) => (
      <span className="flex items-center gap-2.5">
        <PrLink url={getValue()} />
        <Mono tone="ink-3">{getValue()}</Mono>
      </span>
    ),
  }),
]);

const EMPTY_PACKS: AwaitingApprovalDto[] = [];
const EMPTY_IMPORTS: PendingImportDto[] = [];
const EMPTY_PRS: KeptPrDto[] = [];

export function ApprovalsPage() {
  useLiveEvents();

  const approvals = $api.useQuery('get', '/api/approvals');

  const packs = approvals.data?.awaiting_approval ?? EMPTY_PACKS;
  const keptPrs = approvals.data?.kept_prs ?? EMPTY_PRS;
  const imports = approvals.data?.pending_imports ?? EMPTY_IMPORTS;

  const packTable = useDataTable({
    columns: packColumns,
    data: packs,
    getRowId: (pack) => `${pack.key}-${String(pack.scope_id)}`,
  });

  const importTable = useDataTable({
    columns: importColumns,
    data: imports,
    getRowId: (row) => row.id,
  });

  const prTable = useDataTable({
    columns: prColumns,
    data: keptPrs,
    getRowId: (pr) => `${pr.issue}-${pr.pr_url}`,
  });

  return (
    <>
      <PageHeader
        eyebrow="Queue"
        title="Approvals"
        description="Scope packs waiting on a human, packs proposed for the registry, and the candidate PRs the loop already kept."
      />

      <QueryState query={approvals} noun="APPROVALS">
        <Section>
          <SectionHeader title="Pack approvals" note={`${packs.length} at the checkpoint`} />
          <DataTable
            table={packTable}
            renderSubRow={(row) => <EvidencePanel scopeId={row.original.scope_id} />}
            empty={
              <Empty
                title="NO PACKS AT THE CHECKPOINT"
                description="Nothing is waiting on a human right now."
              />
            }
          />
        </Section>

        <Section>
          <SectionHeader title="Pack imports" note={`${imports.length} proposed`} />
          <DataTable
            table={importTable}
            empty={
              <Empty
                title="NO PACK IMPORTS PROPOSED"
                description="Nothing is waiting to be registered into the playbook registry."
              />
            }
          />
        </Section>

        <Section>
          <SectionHeader title="PR reviews" note={`${keptPrs.length} open`} />
          <DataTable
            table={prTable}
            empty={<Empty title="NO OPEN CANDIDATE PRS" />}
          />
        </Section>
      </QueryState>
    </>
  );
}

// --- measurement evidence ------------------------------------------------------

function EvidencePanel({ scopeId }: { scopeId: number }) {
  const evidence = $api.useQuery('get', '/api/approvals/{scope_id}/evidence', {
    params: { path: { scope_id: scopeId } },
  });

  if (evidence.isPending) {
    return (
      <div className="px-4.5 py-3.5">
        <Spinner label="LOADING EVIDENCE" />
      </div>
    );
  }
  if (evidence.isError) {
    return <p className="m-0 px-4.5 py-3.5 text-ink-2">{formatError(evidence.error)}</p>;
  }

  const { data } = evidence;
  if (data.rounds.length === 0) {
    return <p className="m-0 px-4.5 py-3.5 text-ink-2">No refine trail — a hand-authored pack.</p>;
  }

  const judgeBlock = data.rounds.find((r) => r.judge_block.length > 0)?.judge_block;
  const adversary = lastAdversaryRound(data.rounds);
  const attacks = adversaryAttacks(data.rounds);

  return (
    <div className="grid gap-3.5 px-4.5 py-3.5">
      {data.check_outcome && (
        <div className="flex items-center gap-2.5">
          <Mono size="micro" uppercase tone="ink-3" className="tracking-section">
            Check outcome
          </Mono>
          <Status
            status={data.check_outcome}
            tone={statusTone(data.check_outcome === 'PASS' ? 'green' : 'red')}
          />
        </div>
      )}

      {judgeBlock && (
        <div>
          <Mono size="micro" uppercase tone="ink-3" className="tracking-section">
            [judge]
          </Mono>
          <CodeBlock>{judgeBlock}</CodeBlock>
        </div>
      )}

      <div className="grid gap-2">
        {data.rounds
          .filter((r) => r.kind !== 'adversary')
          .map((round) => (
            <RoundRow key={round.round} round={round} />
          ))}
      </div>

      {adversary &&
        (adversaryPassed(data.rounds) ? (
          <Status status="red-team passed" tone="green" />
        ) : (
          <div className="grid gap-2">
            <Status status="red-team concerns" tone="amber" />
            {attacks.map((attack, i) => (
              <div key={i} className="border-l-2 border-amber px-2.5 py-1">
                <Mono size="micro" weight="bold" uppercase tone="amber">
                  {attack.kind}
                </Mono>
                <p className="mt-1 mb-0 text-ink-2">{attack.narrative}</p>
                <p className="mt-1 mb-0 text-ink-3">suggestion: {attack.suggestion}</p>
              </div>
            ))}
          </div>
        ))}
    </div>
  );
}

function CodeBlock({ children }: { children: string }) {
  return (
    <pre className="mt-1 mb-0 max-h-96 overflow-auto border border-rule bg-surface px-2.5 py-2 font-mono text-data whitespace-pre-wrap text-ink-2">
      {children}
    </pre>
  );
}

function RoundRow({ round }: { round: RoundRecord }) {
  const { label, tone } = roundOutcomeTone(round.outcome);
  const stderrTail = contractStderrTail(round.outcome);
  const selftest = selftestEvidence(round.outcome);
  const [showStderr, setShowStderr] = useState(false);

  return (
    <div className="border border-rule px-2.5 py-2">
      <div className="flex flex-wrap items-center gap-2.5">
        <Mono weight="semibold" tone="ink">
          round {round.round}
        </Mono>
        <Mono size="micro" uppercase tone="ink-3" className="border border-rule-hard px-1">
          {round.kind}
        </Mono>
        <Status status={label} tone={statusTone(tone)} />
      </div>
      <p className="mt-1 mb-0 text-ink-2">{outcomeSummary(round.outcome)}</p>

      {selftest && <SelftestTable direction={selftest.direction} good={selftest.good} bad={selftest.bad} />}

      {stderrTail && stderrTail.length > 0 && (
        <div className="mt-1.5">
          <Button
            className="px-0"
            onClick={() => {
              setShowStderr((v) => !v);
            }}
          >
            {showStderr ? 'HIDE' : 'SHOW'} MEASURE_CMD STDERR
          </Button>
          {showStderr && <CodeBlock>{stderrTail.join('\n')}</CodeBlock>}
        </div>
      )}
    </div>
  );
}

function SelftestTable({
  direction,
  good,
  bad,
}: {
  direction: string;
  good: ControlEvidence;
  bad: ControlEvidence;
}) {
  return (
    <div className="mt-1.5">
      <Mono tone="ink-3">direction: {direction} wins</Mono>
      <table className="mt-1 w-full border-collapse bg-surface">
        <thead>
          <tr>
            <ControlHead>control</ControlHead>
            <ControlHead>cmd</ControlHead>
            <ControlHead align="end">mean</ControlHead>
            <ControlHead>valid</ControlHead>
            <ControlHead>readings</ControlHead>
          </tr>
        </thead>
        <tbody>
          <ControlRow label="good" control={good} />
          <ControlRow label="bad" control={bad} />
        </tbody>
      </table>
    </div>
  );
}

function ControlHead({ children, align }: { children: string; align?: 'end' }) {
  return (
    <th
      className={cn(
        'border-b border-rule-hard bg-sunk px-2 py-1 font-mono text-label font-semibold uppercase tracking-group text-ink-2',
        align === 'end' ? 'text-right' : 'text-left',
      )}
    >
      {children}
    </th>
  );
}

function ControlRow({ label, control }: { label: string; control: ControlEvidence }) {
  const readings = control.readings
    .map((r) => (r.valid ? String(r.score ?? '-') : `invalid(${r.note})`))
    .join(', ');
  return (
    <tr className="font-mono text-data text-ink-2">
      <td className="border-b border-rule px-2 py-1">{label}</td>
      <td className="border-b border-rule px-2 py-1">{control.cmd}</td>
      <td className="border-b border-rule px-2 py-1 text-right">{control.mean.toFixed(4)}</td>
      <td className="border-b border-rule px-2 py-1">{control.all_valid ? 'yes' : 'no'}</td>
      <td className="border-b border-rule px-2 py-1">{readings}</td>
    </tr>
  );
}
