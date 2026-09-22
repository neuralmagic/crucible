/* eslint-disable @typescript-eslint/no-unsafe-member-access */
// The disabled rule fires only on the apache-arrow row boundary below: DuckDB result rows are
// StructRowProxy objects whose index signature is `any`. Everything crossing out of this module is
// re-typed as `QueryResult` (string columns, `unknown` cells) — the untyped surface stays in here.
import * as duckdb from '@duckdb/duckdb-wasm';
import wasmEh from '@duckdb/duckdb-wasm/dist/duckdb-eh.wasm?url';
import workerEh from '@duckdb/duckdb-wasm/dist/duckdb-browser-eh.worker.js?url';

export interface QueryResult {
  columns: string[];
  rows: unknown[][];
  elapsedMs: number;
  truncated: boolean;
}

/// Everything the page needs: the two parquet exports are registered and exposed as the `runs` and
/// `iterations` views, so queries read like `SELECT … FROM runs`.
export interface ExploreDb {
  query(sql: string): Promise<QueryResult>;
}

/// Rows returned to the UI are capped; DuckDB still scans everything (aggregates are exact), only
/// the displayed result set is bounded.
const ROW_CAP = 2_000;

// All wasm/worker assets are same-origin emissions of the Vite build (the `?url` imports above), so
// this works from the rust-embedded SPA with no CDN and no cross-origin worker. EH-only on purpose
// (no selectBundle): the mvp fallback exists for browsers without wasm exception handling, we
// assume modern browsers, and every emitted bundle is another ~40MB baked into the controller
// binary by rust-embed.
const BUNDLE: duckdb.DuckDBBundle = {
  mainModule: wasmEh,
  mainWorker: workerEh,
  pthreadWorker: null,
};

async function fetchExport(name: string): Promise<Uint8Array> {
  const res = await fetch(`/api/export/${name}`);
  if (!res.ok) {
    throw new Error(`fetching /api/export/${name}: ${res.status} ${res.statusText}`);
  }
  return new Uint8Array(await res.arrayBuffer());
}

async function boot(): Promise<ExploreDb> {
  if (BUNDLE.mainWorker === null) {
    throw new Error('DuckDB bundle has no worker');
  }
  const db = new duckdb.AsyncDuckDB(new duckdb.VoidLogger(), new Worker(BUNDLE.mainWorker));
  await db.instantiate(BUNDLE.mainModule, BUNDLE.pthreadWorker);

  const [runs, iterations] = await Promise.all([
    fetchExport('runs.parquet'),
    fetchExport('iterations.parquet'),
  ]);
  await db.registerFileBuffer('runs.parquet', runs);
  await db.registerFileBuffer('iterations.parquet', iterations);

  const setup = await db.connect();
  await setup.query(`CREATE VIEW runs AS SELECT * FROM parquet_scan('runs.parquet')`);
  await setup.query(`CREATE VIEW iterations AS SELECT * FROM parquet_scan('iterations.parquet')`);
  await setup.close();

  return {
    async query(sql: string): Promise<QueryResult> {
      const conn = await db.connect();
      try {
        const started = performance.now();
        const table = await conn.query(sql);
        const elapsedMs = performance.now() - started;
        const columns = table.schema.fields.map((f) => f.name);
        const all = table.toArray();
        const truncated = all.length > ROW_CAP;
        const rows = all.slice(0, ROW_CAP).map((row) => columns.map((c): unknown => row[c]));
        return { columns, rows, elapsedMs, truncated };
      } finally {
        await conn.close();
      }
    },
  };
}

let instance: Promise<ExploreDb> | null = null;

/// Boot once per tab; a failed boot clears the memo so a reload of the page retries instead of
/// caching the rejection forever.
export function exploreDb(): Promise<ExploreDb> {
  if (instance === null) {
    instance = boot().catch((e: unknown) => {
      instance = null;
      throw e;
    });
  }
  return instance;
}

/// Render one result cell for display. Arrow hands back JS primitives, BigInts, Dates, nested
/// vectors, and byte arrays depending on the column type — everything becomes a compact string.
export function formatCell(v: unknown): string {
  if (v === null || v === undefined) {
    return '∅';
  }
  if (typeof v === 'bigint' || typeof v === 'number' || typeof v === 'boolean') {
    return String(v);
  }
  if (typeof v === 'string') {
    return v;
  }
  if (v instanceof Date) {
    return v.toISOString();
  }
  if (v instanceof Uint8Array) {
    return `bytes[${v.length}]`;
  }
  return JSON.stringify(v);
}
