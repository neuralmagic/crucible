import { useEffect, useMemo, useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import { CodeDiff } from '../editor/CodeDiff';
import { CodeSurface, type CodeMarker } from '../editor/CodeSurface';
import { cedarCompletionSource, cedarVocabulary } from '../editor/cedarComplete';
import type { CedarValidator } from '../editor/cedarWasm';
import { viaLabel } from '../ownerContext';
import {
  BareTextInput,
  FormActions,
  FormError,
  FormGrid,
  SelectField,
  TextField,
} from './formControls';
import {
  Button,
  cn,
  createDataColumnHelper,
  DataTable,
  Empty,
  LoadingBlock,
  Mono,
  PageHeader,
  Section,
  SectionBody,
  SectionHeader,
  Spec,
  Status,
  useDataTable,
} from '../ui';
import {
  diffRules,
  explainParams,
  newestFirst,
  parsePolicies,
  ruleAnchor,
  ruleDiffIsEmpty,
  shortDigest,
  type ExplainParams,
  type PolicyRule,
  type PolicySetDto,
} from './policyView';
import { policyMarkers } from './policyCheck';

function useActivate() {
  const qc = useQueryClient();
  const mutation = $api.useMutation('post', '/api/authz/policy-sets/{digest}/activate');
  const activate = async (digest: string) => {
    await mutation.mutateAsync({ params: { path: { digest } } });
    await Promise.all([
      qc.invalidateQueries({ queryKey: ['get', '/api/authz/policy-sets'] }),
      qc.invalidateQueries({ queryKey: ['get', '/api/authz/explain'] }),
      qc.invalidateQueries({ queryKey: ['get', '/api/whoami'] }),
    ]);
  };
  return { activate, pending: mutation.isPending };
}

function RuleDiffSummary({ before, after }: { before: string; after: string }) {
  const diff = diffRules(parsePolicies(before), parsePolicies(after));
  if (ruleDiffIsEmpty(diff)) return <Status status="no rule changes" tone="grey" />;
  const groups = [
    { label: 'added', tone: 'green', ids: diff.added },
    { label: 'changed', tone: 'amber', ids: diff.changed },
    { label: 'removed', tone: 'red', ids: diff.removed },
  ] as const;
  return (
    <dl className="m-0 grid gap-1" data-testid="rule-diff">
      {groups
        .filter((group) => group.ids.length > 0)
        .map((group) => (
          <div key={group.label} className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
            <dt>
              <Status status={`${group.label} ${group.ids.length}`} tone={group.tone} />
            </dt>
            {group.ids.map((id) => (
              <dd key={id} className="m-0">
                <Mono size="data">{id}</Mono>
              </dd>
            ))}
          </div>
        ))}
    </dl>
  );
}

interface SetDiffProps {
  active: PolicySetDto;
  other: PolicySetDto;
  /// Keeps two open diffs of the same digest on separate Monaco models.
  scope: string;
}

function SetDiff({ active, other, scope }: SetDiffProps) {
  const { activate, pending } = useActivate();
  const [error, setError] = useState<string | null>(null);
  const current = other.digest === active.digest;
  return (
    <>
      <SectionBody className="flex items-start gap-4">
        <div className="min-w-0 flex-1">
          <RuleDiffSummary before={active.text} after={other.text} />
        </div>
        {current ? (
          <Status status="active" tone="green" />
        ) : (
          <Button
            variant="filled"
            disabled={pending}
            data-testid={`activate-${other.digest}`}
            onClick={() => {
              setError(null);
              activate(other.digest).catch((err: unknown) => {
                setError(formatError(err));
              });
            }}
          >
            {pending ? 'ACTIVATING…' : 'ACTIVATE'}
          </Button>
        )}
      </SectionBody>
      {error === null ? null : (
        <SectionBody>
          <FormError>{error}</FormError>
        </SectionBody>
      )}
      <SectionBody padded={false}>
        {current ? (
          <CodeSurface path={`policy/${scope}/${other.digest}.cedar`} value={other.text} readOnly height="24rem" />
        ) : (
          <CodeDiff path={`policy/${scope}/${other.digest}.cedar`} original={active.text} modified={other.text} height="24rem" />
        )}
      </SectionBody>
    </>
  );
}

const EFFECT_TONE = { permit: 'green', forbid: 'red' } as const;

interface ActiveSetProps {
  active: PolicySetDto;
  rules: readonly PolicyRule[];
  highlighted: string | null;
}

function ActiveSet({ active, rules, highlighted }: ActiveSetProps) {
  const [filter, setFilter] = useState('');
  const needle = filter.trim().toLowerCase();
  const shown = needle.length === 0 ? rules : rules.filter((rule) => rule.text.toLowerCase().includes(needle));
  return (
    <Section>
      <SectionHeader
        title="Active set"
        note={<Mono size="label">{shortDigest(active.digest)}</Mono>}
        actions={
          <BareTextInput
            id="rule-filter"
            aria-label="Filter rules"
            placeholder="filter"
            mono
            value={filter}
            onChange={setFilter}
            className="w-56 py-0.5"
          />
        }
      />
      <SectionBody>
        <Spec
          items={[
            { label: 'Digest', value: <span title={active.digest}>{shortDigest(active.digest)}</span> },
            { label: 'Activated by', value: active.activated_by ?? '—' },
            { label: 'Activated at', value: active.activated_at ?? '—' },
            { label: 'Rules', value: shown.length, note: shown.length === rules.length ? undefined : `/ ${rules.length}` },
          ]}
        />
      </SectionBody>
      <ol className="m-0 list-none bg-surface p-0" data-testid="active-rules">
        {shown.map((rule) => (
          <li
            key={rule.id ?? rule.text}
            id={rule.id === null ? undefined : ruleAnchor(rule.id)}
            className={cn(
              'scroll-mt-4 border-t border-rule px-4.5 py-2',
              rule.id !== null && rule.id === highlighted && 'bg-hi',
            )}
          >
            <div className="flex items-baseline gap-3">
              <Mono size="data" weight="semibold" tone="ink">
                {rule.id ?? '(no @id)'}
              </Mono>
              {rule.effect === null ? null : <Status status={rule.effect} tone={EFFECT_TONE[rule.effect]} />}
              <Mono size="label" tone="ink-3" className="ml-auto">
                L{rule.line}
              </Mono>
            </div>
            <pre className="m-0 mt-1 overflow-x-auto font-mono text-data whitespace-pre text-ink-2">{rule.text}</pre>
          </li>
        ))}
      </ol>
      {shown.length === 0 ? <Empty title={rules.length === 0 ? 'NO RULES' : 'NO MATCHING RULES'} /> : null}
    </Section>
  );
}

/// Early feedback only; the server validates every save, so a missing schema or validator yields no
/// markers.
function usePolicyCheck(text: string, schema: string | undefined): CodeMarker[] {
  const [validator, setValidator] = useState<CedarValidator | null>(null);
  const [markers, setMarkers] = useState<CodeMarker[]>([]);

  useEffect(() => {
    let live = true;
    import('../editor/cedarWasm')
      .then((module) => module.loadCedar())
      .then((loaded) => {
        if (live) setValidator(loaded);
      })
      .catch(() => undefined);
    return () => {
      live = false;
    };
  }, []);

  useEffect(() => {
    if (validator === null || schema === undefined) return;
    const timer = setTimeout(() => {
      setMarkers(policyMarkers(validator.validate(text, schema), text));
    }, 250);
    return () => {
      clearTimeout(timer);
    };
  }, [validator, schema, text]);

  return markers;
}

function Editor({ active }: { active: PolicySetDto }) {
  const qc = useQueryClient();
  const schema = $api.useQuery('get', '/api/authz/schema', { parseAs: 'text' }, { staleTime: Infinity });
  const actions = $api.useQuery('get', '/api/authz/actions');
  const create = $api.useMutation('post', '/api/authz/policy-sets');
  const [draft, setDraft] = useState<string | null>(null);
  const [saved, setSaved] = useState<PolicySetDto | null>(null);
  const [error, setError] = useState<string | null>(null);
  const text = draft ?? active.text;
  const dirty = text !== active.text;
  const markers = usePolicyCheck(text, schema.data);
  const completions = useMemo(
    () => cedarCompletionSource(cedarVocabulary(actions.data ?? [], schema.data ?? '')),
    [actions.data, schema.data],
  );

  const save = async () => {
    setError(null);
    try {
      const stored = await create.mutateAsync({ body: { text } });
      setSaved(stored);
      await qc.invalidateQueries({ queryKey: ['get', '/api/authz/policy-sets'] });
    } catch (err: unknown) {
      setSaved(null);
      setError(formatError(err));
    }
  };

  return (
    <>
      <Section>
        <SectionHeader
          title="Edit"
          note={dirty ? <Status status="modified" tone="amber" /> : undefined}
          actions={
            <Button
              disabled={!dirty || create.isPending}
              onClick={() => {
                setDraft(null);
                setSaved(null);
                setError(null);
              }}
            >
              RESET
            </Button>
          }
        />
        <SectionBody padded={false}>
          <CodeSurface
            path="policy/edit.cedar"
            value={text}
            onChange={setDraft}
            markers={markers}
            completions={completions}
            onSave={() => {
              if (dirty && !create.isPending) void save();
            }}
            height="28rem"
            label="Policy text"
            testId="policy-editor"
          />
        </SectionBody>
        {error === null ? null : (
          <SectionBody>
            <FormError>{error}</FormError>
          </SectionBody>
        )}
        <FormActions>
          <Button
            variant="filled"
            disabled={!dirty || create.isPending}
            onClick={() => {
              void save();
            }}
          >
            {create.isPending ? 'SAVING…' : 'SAVE VERSION'}
          </Button>
          {markers.length === 0 ? null : (
            <span data-testid="policy-errors">
              <Status status={`${markers.length} ${markers.length === 1 ? 'error' : 'errors'}`} tone="red" />
            </span>
          )}
        </FormActions>
      </Section>
      {saved === null ? null : (
        <Section>
          <SectionHeader
            title="Stored"
            note={<Mono size="label">{shortDigest(saved.digest)}</Mono>}
            actions={
              <Button
                onClick={() => {
                  setSaved(null);
                }}
              >
                CLOSE
              </Button>
            }
          />
          <SetDiff active={active} other={saved} scope="stored" />
        </Section>
      )}
    </>
  );
}

const helper = createDataColumnHelper<PolicySetDto>();

const historyColumns = helper.columns([
  helper.accessor('digest', {
    header: 'Digest',
    enableSorting: false,
    meta: { pad: 'tight', shrink: true },
    cell: ({ getValue }) => (
      <Mono size="data" title={getValue()}>
        {shortDigest(getValue())}
      </Mono>
    ),
  }),
  helper.accessor('active', {
    header: 'State',
    enableSorting: false,
    meta: { shrink: true },
    cell: ({ getValue }) => (getValue() ? <Status status="active" tone="green" /> : <Status status="stored" tone="grey" />),
  }),
  helper.accessor('text', {
    header: 'Rules',
    enableSorting: false,
    meta: { shrink: true, align: 'end', className: 'font-mono text-data' },
    cell: ({ getValue }) => parsePolicies(getValue()).length,
  }),
  helper.accessor('created_by', {
    header: 'Created by',
    enableSorting: false,
    meta: { className: 'font-mono text-data text-ink-2' },
    cell: ({ getValue }) => getValue() ?? '—',
  }),
  helper.accessor('created_at', {
    header: 'Created',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data text-ink-3' },
  }),
  helper.accessor('activated_at', {
    header: 'Last activated',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data text-ink-3' },
    cell: ({ row, getValue }) => {
      const at = getValue();
      if (at === null || at === undefined) return '—';
      return row.original.activated_by ? `${at} by ${row.original.activated_by}` : at;
    },
  }),
]);

function History({ active, sets }: { active: PolicySetDto; sets: readonly PolicySetDto[] }) {
  const rows = useMemo(() => newestFirst(sets), [sets]);
  const table = useDataTable({
    columns: historyColumns,
    data: rows,
    getRowId: (row) => row.digest,
  });
  return (
    <Section>
      <SectionHeader title="History" note={`${rows.length} stored`} />
      <div data-testid="policy-history">
        <DataTable
          table={table}
          empty={<Empty title="NO STORED SETS" />}
          renderSubRow={(row) => <SetDiff active={active} other={row.original} scope="history" />}
        />
      </div>
    </Section>
  );
}

interface ExplainProps {
  rules: readonly PolicyRule[];
  onRule: (id: string) => void;
}

function Explain({ rules, onRule }: ExplainProps) {
  const actions = $api.useQuery('get', '/api/authz/actions');
  const [login, setLogin] = useState('');
  const [action, setAction] = useState('autoresearch:access');
  const [resource, setResource] = useState('');
  const [asked, setAsked] = useState<ExplainParams | null>(null);
  const answer = $api.useQuery(
    'get',
    '/api/authz/explain',
    { params: { query: asked ?? { login: '', action: '' } } },
    { enabled: asked !== null, retry: false },
  );
  const known = new Set(rules.flatMap((rule) => (rule.id === null ? [] : [rule.id])));
  const options = (actions.data ?? []).map((a) => ({ value: a.action, label: a.action }));
  const chosen = options.some((o) => o.value === action) ? action : (options[0]?.value ?? '');
  const ready = explainParams(login, chosen, resource);

  return (
    <Section>
      <SectionHeader title="Explain" />
      <SectionBody>
        <form
          onSubmit={(event) => {
            event.preventDefault();
            setAsked(ready);
          }}
        >
          <FormGrid className="grid-cols-[1fr_1fr_1fr_auto] items-end max-w-none">
            <TextField id="explain-login" label="Login" mono required value={login} onChange={setLogin} />
            <SelectField id="explain-action" label="Action" value={chosen} onChange={setAction} options={options} />
            <TextField id="explain-resource" label="Resource" mono value={resource} onChange={setResource} />
            <Button type="submit" variant="filled" disabled={ready === null || answer.isFetching}>
              {answer.isFetching ? 'DECIDING…' : 'EXPLAIN'}
            </Button>
          </FormGrid>
        </form>
      </SectionBody>
      {asked === null ? null : answer.isError ? (
        <SectionBody>
          <FormError>{formatError(answer.error)}</FormError>
        </SectionBody>
      ) : answer.isPending ? (
        <LoadingBlock label="DECIDING" />
      ) : (
        <SectionBody className="grid gap-3">
          <div className="flex flex-wrap items-center gap-4">
            <Status status={answer.data.allowed ? 'allowed' : 'denied'} tone={answer.data.allowed ? 'green' : 'red'} />
            <Mono size="data" tone="ink">
              {answer.data.login} · {answer.data.action} · {answer.data.resource}
            </Mono>
            <Mono size="data" tone="ink-3">
              owner {answer.data.owner} · set {shortDigest(answer.data.policy)}
            </Mono>
          </div>
          <dl className="m-0 grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1.5">
            <dt>
              <Mono size="label" tone="ink-3" uppercase>
                Rules
              </Mono>
            </dt>
            <dd className="m-0 flex flex-wrap gap-2">
              {answer.data.rules.length === 0 ? (
                <Mono size="data" tone="ink-3">
                  no-rule
                </Mono>
              ) : (
                answer.data.rules.map((id) =>
                  known.has(id) ? (
                    <Button
                      key={id}
                      className="px-1 py-0 underline"
                      onClick={() => {
                        onRule(id);
                      }}
                    >
                      {id}
                    </Button>
                  ) : (
                    <Mono key={id} size="data">
                      {id}
                    </Mono>
                  ),
                )
              )}
            </dd>
            <dt>
              <Mono size="label" tone="ink-3" uppercase>
                Groups
              </Mono>
            </dt>
            <dd className="m-0 flex flex-wrap gap-x-3 gap-y-1">
              {answer.data.groups.length === 0 ? (
                <Mono size="data" tone="ink-3">
                  none
                </Mono>
              ) : (
                answer.data.groups.map((group) => (
                  <Mono key={group} size="data">
                    {group}
                  </Mono>
                ))
              )}
              {answer.data.groups_at ? (
                <Mono size="data" tone="ink-3">
                  as of {answer.data.groups_at}
                </Mono>
              ) : null}
            </dd>
            <dt>
              <Mono size="label" tone="ink-3" uppercase>
                Teams
              </Mono>
            </dt>
            <dd className="m-0 grid gap-1">
              {answer.data.teams.length === 0 ? (
                <Mono size="data" tone="ink-3">
                  none
                </Mono>
              ) : (
                answer.data.teams.map((team) => (
                  <Mono key={team.team} size="data">
                    {team.team} · {team.role}
                    <span className="text-ink-3"> ({team.via.map(viaLabel).join(', ')})</span>
                  </Mono>
                ))
              )}
            </dd>
          </dl>
        </SectionBody>
      )}
    </Section>
  );
}

/// The Cedar policy set in force, its history, a way to store and activate a new version, and
/// the decision it makes for any signed-in user.
export function PolicyPage() {
  const whoami = $api.useQuery('get', '/api/whoami');
  const admin = whoami.data?.role === 'admin';
  const sets = $api.useQuery('get', '/api/authz/policy-sets', {}, { enabled: admin });
  const [highlighted, setHighlighted] = useState<string | null>(null);
  const active = sets.data?.find((set) => set.active) ?? null;
  const rules = useMemo(() => (active === null ? [] : parsePolicies(active.text)), [active]);

  const header = <PageHeader eyebrow="System" title="Policy" />;
  if (whoami.isPending) return <LoadingBlock label="LOADING" />;
  if (!admin) {
    return (
      <>
        {header}
        <Empty title="PLATFORM ADMINISTRATORS ONLY" />
      </>
    );
  }
  if (sets.isError) {
    return (
      <>
        {header}
        <Empty title="POLICY UNAVAILABLE" description={formatError(sets.error)} />
      </>
    );
  }
  if (sets.isPending) {
    return (
      <>
        {header}
        <LoadingBlock label="LOADING POLICY" />
      </>
    );
  }
  if (active === null) {
    return (
      <>
        {header}
        <Empty title="NO ACTIVE SET" />
      </>
    );
  }

  return (
    <>
      {header}
      <ActiveSet active={active} rules={rules} highlighted={highlighted} />
      <Editor key={active.digest} active={active} />
      <History active={active} sets={sets.data} />
      <Explain
        rules={rules}
        onRule={(id) => {
          setHighlighted(id);
          document.getElementById(ruleAnchor(id))?.scrollIntoView({ behavior: 'smooth', block: 'start' });
        }}
      />
    </>
  );
}
