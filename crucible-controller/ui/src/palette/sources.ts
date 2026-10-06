export type Action = { kind: 'navigate'; path: string } | { kind: 'external'; url: string };

export type Command = {
  id: string;
  title: string;
  subtitle?: string;
  keywords: string[];
  action: Action;
};

export type Source = {
  group: string;
  load: () => Promise<Command[]>;
};

export type FetchText = (url: string) => Promise<string>;

export const DOCS_URL = 'https://neuralmagic.github.io/crucible/';

const PAGES: ReadonlyArray<readonly [title: string, path: string, keywords: string[]]> = [
  ['Home', '/', ['dashboard', 'overview']],
  ['Approvals', '/approvals', ['review', 'pending']],
  ['Playbooks', '/playbooks', ['registry', 'packs']],
  ['Drafts', '/playbooks/drafts', ['studio', 'authoring']],
  ['Import a playbook', '/playbooks/import', ['add', 'register']],
  ['Playbook runs', '/playbook-runs', ['launches', 'history']],
  ['Schedules', '/schedules', ['cron', 'standing']],
  ['Webhooks', '/webhooks', ['triggers', 'hooks']],
  ['Activity', '/activity', ['events', 'log']],
  ['Teams', '/teams', ['members', 'groups']],
  ['Secrets', '/secrets', ['credentials', 'keys']],
  ['Providers', '/providers', ['models', 'inference']],
  ['Settings', '/settings', ['api key', 'preferences']],
  ['Admin', '/admin', ['platform']],
  ['Policy', '/policy', ['authorization', 'cedar']],
];

const segment = (value: string) => encodeURIComponent(value);

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === 'object' && value !== null && !Array.isArray(value);

const text = (record: Record<string, unknown>, key: string): string | null => {
  const value = record[key];
  return typeof value === 'string' ? value : null;
};

const number = (record: Record<string, unknown>, key: string): number | null => {
  const value = record[key];
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
};

/** Rows of a list response that carry every field `pick` needs; anything else is skipped. */
const rows = <T>(body: unknown, pick: (record: Record<string, unknown>) => T | null): T[] =>
  Array.isArray(body)
    ? body.flatMap((item) => {
        const row = isRecord(item) ? pick(item) : null;
        return row === null ? [] : [row];
      })
    : [];

export type Playbook = { id: string; description: string };
export type Launch = {
  key: string;
  playbook: string;
  status: string;
  createdAt: string;
  costUsd: number | null;
  draftVersion: number | null;
};
export type Draft = { id: string; description: string; latestVersion: number };

export const playbooks = (body: unknown): Playbook[] =>
  rows(body, (r) => {
    const id = text(r, 'id');
    return id === null ? null : { id, description: text(r, 'description') ?? '' };
  });

export const launches = (body: unknown): Launch[] =>
  rows(body, (r) => {
    const key = text(r, 'key');
    const playbook = text(r, 'playbook');
    const status = text(r, 'status');
    const createdAt = text(r, 'created_at');
    if (key === null || playbook === null || status === null || createdAt === null) return null;
    return {
      key,
      playbook,
      status,
      createdAt,
      costUsd: number(r, 'cost_usd'),
      draftVersion: number(r, 'draft_version'),
    };
  });

export const drafts = (body: unknown): Draft[] =>
  rows(body, (r) => {
    const id = text(r, 'id');
    const latestVersion = number(r, 'latest_version');
    if (id === null || latestVersion === null) return null;
    return { id, description: text(r, 'description') ?? '', latestVersion };
  });

export const pageCommands = (): Command[] =>
  PAGES.map(([title, path, keywords]) => ({
    id: `page:${path}`,
    title,
    keywords,
    action: { kind: 'navigate', path },
  }));

export const playbookCommands = (list: Playbook[]): Command[] =>
  list.flatMap((p) => [
    {
      id: `launch:${p.id}`,
      title: `Launch ${p.id}`,
      subtitle: p.description,
      keywords: ['run', 'start', 'fire'],
      action: { kind: 'navigate', path: `/playbooks/${segment(p.id)}/launch` },
    },
    {
      id: `playbook:${p.id}`,
      title: p.id,
      subtitle: p.description,
      keywords: ['playbook'],
      action: { kind: 'navigate', path: `/playbooks/${segment(p.id)}` },
    },
  ]);

const money = (usd: number | null) => (usd === null ? null : `$${usd.toFixed(2)}`);

export const launchCommands = (list: Launch[], limit = 25): Command[] =>
  list.slice(0, limit).map((l) => ({
    id: `run:${l.key}`,
    title: l.playbook,
    subtitle: [
      l.status,
      l.draftVersion === null ? null : `draft v${l.draftVersion}`,
      money(l.costUsd),
      l.createdAt.slice(0, 16).replace('T', ' '),
    ]
      .filter((part) => part !== null)
      .join(' · '),
    keywords: ['run', l.status, l.key],
    action: { kind: 'navigate', path: `/playbook-runs/${segment(l.key)}` },
  }));

export const draftCommands = (list: Draft[]): Command[] =>
  list.map((d) => ({
    id: `draft:${d.id}`,
    title: `Edit ${d.id}`,
    subtitle: `v${d.latestVersion}${d.description ? ` · ${d.description}` : ''}`,
    keywords: ['draft', 'studio', d.id],
    action: { kind: 'navigate', path: `/playbooks/drafts/${segment(d.id)}` },
  }));

/** The list reads the controller sources need: `GET /api/playbooks`, `/api/playbook-drafts`, `/api/playbook-runs`. */
export type Lists = {
  playbooks: () => Promise<unknown>;
  drafts: () => Promise<unknown>;
  launches: () => Promise<unknown>;
};

/** Everything the controller UI can offer, read through the caller's own session. */
export const controllerSources = (lists: Lists): Source[] => [
  { group: 'Pages', load: () => Promise.resolve(pageCommands()) },
  { group: 'Playbooks', load: async () => playbookCommands(playbooks(await lists.playbooks())) },
  { group: 'Drafts', load: async () => draftCommands(drafts(await lists.drafts())) },
  { group: 'Recent runs', load: async () => launchCommands(launches(await lists.launches())) },
];

export type DocEntry = { title: string; breadcrumbs: string; url: string };

/**
 * The entries of an mdBook `searchindex.js`: one per heading, with its breadcrumbs and URL relative
 * to the book. The file is `window.search = Object.assign(window.search, JSON.parse('<json>'))`,
 * with the JSON escaped as a single-quoted JS string.
 */
export const docEntries = (script: string): DocEntry[] => {
  const literal = /JSON\.parse\('([\s\S]*)'\)\);?\s*$/.exec(script)?.[1];
  if (literal === undefined) return [];
  let index: unknown;
  try {
    index = JSON.parse(literal.replace(/\\([\s\S])/g, '$1'));
  } catch {
    return [];
  }
  if (!isRecord(index) || !Array.isArray(index.doc_urls)) return [];
  const urls = index.doc_urls;
  const store =
    isRecord(index.index) && isRecord(index.index.documentStore) ? index.index.documentStore : null;
  const docs = store !== null && isRecord(store.docs) ? store.docs : {};
  return Object.entries(docs).flatMap(([id, doc]) => {
    const url: unknown = urls[Number(id)];
    if (!isRecord(doc) || typeof url !== 'string') return [];
    const title = text(doc, 'title');
    if (title === null || title === '') return [];
    return [{ title, breadcrumbs: text(doc, 'breadcrumbs') ?? '', url }];
  });
};

export const docCommands = (entries: DocEntry[], base: string): Command[] =>
  entries.map((e) => ({
    id: `doc:${e.url}`,
    title: e.title,
    subtitle: e.breadcrumbs,
    keywords: ['docs', 'documentation', e.breadcrumbs],
    action: { kind: 'external', url: new URL(e.url, base).toString() },
  }));

/** The published book's headings. The index is large, so a caller should keep the source it made. */
export const docsSource = (fetchText: FetchText, base: string = DOCS_URL): Source => {
  let loaded: Promise<Command[]> | null = null;
  return {
    group: 'Docs',
    load: () => {
      loaded ??= fetchText(new URL('searchindex.js', base).toString()).then((script) =>
        docCommands(docEntries(script), base)
      );
      loaded.catch(() => {
        loaded = null;
      });
      return loaded;
    },
  };
};
