import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { exploreDb, formatCell, type QueryResult } from '../explore/db';
import {
  Button,
  createDataColumnHelper,
  DataTable,
  Empty,
  LoadingBlock,
  Mono,
  PageHeader,
  Section,
  SectionBody,
  SectionHeader,
  Toolbar,
  ToolbarActions,
  ToolbarGroup,
  useDataTable,
} from '../ui';

/// Canned starting points; each is one click away so the page is useful before anyone learns the
/// schema. Columns come from the /api/export parquet schemas (pinned to the old report sidecars).
const EXAMPLES: { label: string; sql: string }[] = [
  {
    label: 'Leaderboard',
    sql: `SELECT run_id, issue_key, status, best, cost_usd, iterations, kept\nFROM runs\nORDER BY best ASC\nLIMIT 25`,
  },
  {
    label: 'Spend by status',
    sql: `SELECT status, count(*) AS runs, round(sum(cost_usd), 2) AS usd\nFROM runs\nGROUP BY status\nORDER BY usd DESC`,
  },
  {
    label: 'Keep rate by lane',
    sql: `SELECT lane, decision, count(*) AS n\nFROM iterations\nGROUP BY lane, decision\nORDER BY lane, n DESC`,
  },
  {
    label: 'Score trajectories',
    sql: `SELECT i.run_id, i.iter, i.score, i.decision\nFROM iterations i\nJOIN runs r USING (run_id)\nWHERE r.kept > 0\nORDER BY i.run_id, i.iter\nLIMIT 200`,
  },
];

type Phase =
  | { state: 'booting' }
  | { state: 'ready' }
  | { state: 'running' }
  | { state: 'failed'; error: string };

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function isNumericColumn(rows: unknown[][], index: number): boolean {
  for (const row of rows) {
    const cell = row[index];
    if (cell === null || cell === undefined) continue;
    return typeof cell === 'number' || typeof cell === 'bigint';
  }
  return false;
}

const helper = createDataColumnHelper<unknown[]>();

function ResultTable({ result }: { result: QueryResult }) {
  const columns = useMemo(
    () =>
      result.columns.map((name, index) =>
        helper.display({
          id: `col-${String(index)}`,
          header: name,
          meta: {
            align: isNumericColumn(result.rows, index) ? 'end' : 'start',
            className: 'font-mono text-data text-ink-2',
          },
          cell: ({ row }) => formatCell(row.original[index]),
        }),
      ),
    [result],
  );

  const table = useDataTable({
    columns,
    data: result.rows,
    getRowId: (_row, index) => String(index),
  });

  return (
    <DataTable
      table={table}
      empty={<Empty title="NO ROWS" description="The query returned nothing." />}
      footer={
        <>
          {result.rows.length}
          {result.truncated ? '+ (display capped)' : ''} row
          {result.rows.length === 1 ? '' : 's'} in {result.elapsedMs.toFixed(0)}ms
        </>
      }
    />
  );
}

export default function ExplorePage() {
  const [phase, setPhase] = useState<Phase>({ state: 'booting' });
  const [sql, setSql] = useState(EXAMPLES[0].sql);
  const [result, setResult] = useState<QueryResult | null>(null);
  const [queryError, setQueryError] = useState<string | null>(null);
  const sqlRef = useRef(sql);
  sqlRef.current = sql;

  useEffect(() => {
    let cancelled = false;
    exploreDb().then(
      () => {
        if (!cancelled) setPhase({ state: 'ready' });
      },
      (e: unknown) => {
        if (!cancelled) setPhase({ state: 'failed', error: errorText(e) });
      },
    );
    return () => {
      cancelled = true;
    };
  }, []);

  const run = useCallback(() => {
    setPhase({ state: 'running' });
    setQueryError(null);
    exploreDb()
      .then((db) => db.query(sqlRef.current))
      .then(
        (r) => {
          setResult(r);
          setPhase({ state: 'ready' });
        },
        (e: unknown) => {
          setQueryError(errorText(e));
          setPhase({ state: 'ready' });
        },
      );
  }, []);

  const header = (
    <PageHeader
      eyebrow="Records"
      title="Explore"
      description={
        <>
          Ad-hoc SQL over the run history, entirely in your browser (DuckDB-WASM over{' '}
          <Mono>/api/export/runs.parquet</Mono> + <Mono>iterations.parquet</Mono>, views{' '}
          <Mono>runs</Mono> and <Mono>iterations</Mono>). Nothing you run here leaves the tab.
        </>
      }
    />
  );

  if (phase.state === 'booting') {
    return (
      <>
        {header}
        <LoadingBlock label="BOOTING DUCKDB" />
      </>
    );
  }

  if (phase.state === 'failed') {
    return (
      <>
        {header}
        <Empty title="EXPLORE FAILED TO START" description={phase.error} />
      </>
    );
  }

  const running = phase.state === 'running';

  return (
    <>
      {header}

      <Toolbar>
        <ToolbarGroup label="Examples">
          {EXAMPLES.map((example) => (
            <Button
              key={example.label}
              selected={example.sql === sql}
              onClick={() => {
                setSql(example.sql);
              }}
            >
              {example.label}
            </Button>
          ))}
        </ToolbarGroup>
        <ToolbarActions className="ml-auto">
          <Button variant="filled" disabled={running} onClick={run}>
            {running ? 'RUNNING…' : 'RUN (⌘⏎)'}
          </Button>
        </ToolbarActions>
      </Toolbar>

      <Section>
        <SectionHeader title="Query" note="⌘⏎ to run" />
        <SectionBody padded={false}>
          <textarea
            aria-label="SQL query"
            value={sql}
            onChange={(e) => {
              setSql(e.target.value);
            }}
            onKeyDown={(e) => {
              if ((e.ctrlKey || e.metaKey) && e.key === 'Enter' && !running) run();
            }}
            spellCheck={false}
            rows={Math.min(12, Math.max(4, sql.split('\n').length + 1))}
            className="block w-full resize-y border-0 bg-surface px-4.5 py-3 font-mono text-data-lg leading-relaxed text-ink"
          />
        </SectionBody>
      </Section>

      {queryError !== null && (
        <Section>
          <SectionHeader title="Query failed" className="text-red" />
          <SectionBody>
            <pre className="m-0 font-mono text-data whitespace-pre-wrap text-red">{queryError}</pre>
          </SectionBody>
        </Section>
      )}

      {result && queryError === null && (
        <Section>
          <SectionHeader title="Results" />
          <div className="overflow-x-auto">
            <ResultTable result={result} />
          </div>
        </Section>
      )}
    </>
  );
}
