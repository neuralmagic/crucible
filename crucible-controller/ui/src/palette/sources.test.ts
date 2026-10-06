import { describe, expect, it } from 'vitest';

import {
  type Action,
  controllerSources,
  docCommands,
  docEntries,
  docsSource,
  draftCommands,
  drafts,
  launchCommands,
  launches,
  pageCommands,
  playbookCommands,
  playbooks,
} from './sources';

const target = (a: Action) => {
  switch (a.kind) {
    case 'navigate':
      return a.path;
    case 'external':
      return a.url;
  }
};
const paths = (actions: Action[]) => actions.map(target);

describe('decoders', () => {
  it('keep complete rows and skip malformed ones', () => {
    expect(
      playbooks([
        { id: 'cve', description: 'triage' },
        { description: 'no id' },
        null,
        7,
        { id: 3 },
      ])
    ).toEqual([{ id: 'cve', description: 'triage' }]);
    expect(playbooks([{ id: 'bare' }])).toEqual([{ id: 'bare', description: '' }]);
    expect(
      launches([
        {
          key: 'playbook:cve:1',
          playbook: 'cve',
          status: 'running',
          created_at: '2026-10-06T01:02:03Z',
          cost_usd: 1.5,
        },
        {
          key: 'playbook:cve:2',
          playbook: 'cve',
          status: 'done',
          created_at: '2026-10-06T01:02:03Z',
          cost_usd: null,
        },
        { key: 'missing-status', playbook: 'cve', created_at: 'x' },
      ])
    ).toEqual([
      {
        key: 'playbook:cve:1',
        playbook: 'cve',
        status: 'running',
        createdAt: '2026-10-06T01:02:03Z',
        costUsd: 1.5,
        draftVersion: null,
      },
      {
        key: 'playbook:cve:2',
        playbook: 'cve',
        status: 'done',
        createdAt: '2026-10-06T01:02:03Z',
        costUsd: null,
        draftVersion: null,
      },
    ]);
    expect(
      drafts([
        { id: 'notes', latest_version: 4 },
        { id: 'x', latest_version: '4' },
      ])
    ).toEqual([{ id: 'notes', description: '', latestVersion: 4 }]);
  });

  it('return nothing for a body that is not a list', () => {
    for (const body of [null, undefined, {}, 'x', { items: [] }]) {
      expect(playbooks(body)).toEqual([]);
      expect(launches(body)).toEqual([]);
      expect(drafts(body)).toEqual([]);
    }
  });

  it('refuse non-finite numbers', () => {
    expect(drafts([{ id: 'x', latest_version: Number.NaN }])).toEqual([]);
    expect(
      launches([{ key: 'k', playbook: 'p', status: 's', created_at: 'c', cost_usd: Infinity }])[0]
        ?.costUsd
    ).toBeNull();
  });
});

describe('commands', () => {
  it('give every page a unique id and an absolute path', () => {
    const pages = pageCommands();
    expect(new Set(pages.map((p) => p.id)).size).toBe(pages.length);
    for (const path of paths(pages.map((p) => p.action))) expect(path.startsWith('/')).toBe(true);
  });

  it('offer launch and open for each playbook, encoding the id', () => {
    const commands = playbookCommands([{ id: 'a b/c', description: 'd' }]);
    expect(commands.map((c) => c.title)).toEqual(['Launch a b/c', 'a b/c']);
    expect(paths(commands.map((c) => c.action))).toEqual([
      '/playbooks/a%20b%2Fc/launch',
      '/playbooks/a%20b%2Fc',
    ]);
  });

  it('summarize launches and cap how many are offered', () => {
    const list = Array.from({ length: 30 }, (_, i) => ({
      key: `playbook:cve:${i}`,
      playbook: 'cve',
      status: 'done',
      createdAt: '2026-10-06T01:02:03Z',
      costUsd: i === 0 ? 3.14159 : null,
      draftVersion: i === 1 ? 4 : null,
    }));
    const commands = launchCommands(list);
    expect(commands).toHaveLength(25);
    expect(commands[0]?.subtitle).toBe('done · $3.14 · 2026-10-06 01:02');
    expect(commands[1]?.subtitle).toBe('done · draft v4 · 2026-10-06 01:02');
    expect(commands[2]?.subtitle).toBe('done · 2026-10-06 01:02');
    expect(paths(commands.slice(0, 1).map((c) => c.action))).toEqual([
      '/playbook-runs/playbook%3Acve%3A0',
    ]);
  });

  it("show a draft's version and description", () => {
    const [withDescription, without] = draftCommands([
      { id: 'notes', description: 'release notes', latestVersion: 3 },
      { id: 'bare', description: '', latestVersion: 1 },
    ]);
    expect(withDescription?.title).toBe('Edit notes');
    expect(withDescription?.subtitle).toBe('v3 · release notes');
    expect(without?.subtitle).toBe('v1');
    expect(paths(withDescription ? [withDescription.action] : [])).toEqual([
      '/playbooks/drafts/notes',
    ]);
  });
});

describe('controllerSources', () => {
  it('read each list once through the given loaders', async () => {
    const asked: string[] = [];
    const bodies: Record<string, unknown> = {
      '/api/playbooks': [{ id: 'cve', description: '' }],
      '/api/playbook-runs': [
        { key: 'k', playbook: 'cve', status: 'running', created_at: '2026-10-06T00:00:00Z' },
      ],
      '/api/playbook-drafts': [{ id: 'notes', latest_version: 2 }],
    };
    const read = (path: string) => () => {
      asked.push(path);
      return Promise.resolve(bodies[path]);
    };
    const sources = controllerSources({
      playbooks: read('/api/playbooks'),
      drafts: read('/api/playbook-drafts'),
      launches: read('/api/playbook-runs'),
    });
    const loaded = await Promise.all(sources.map(async (s) => [s.group, (await s.load()).length]));
    expect(loaded).toEqual([
      ['Pages', pageCommands().length],
      ['Playbooks', 2],
      ['Drafts', 1],
      ['Recent runs', 1],
    ]);
    expect(asked.sort()).toEqual(['/api/playbook-drafts', '/api/playbook-runs', '/api/playbooks']);
  });

  it('let a failing endpoint reject only its own group', async () => {
    const sources = controllerSources({
      playbooks: () => Promise.reject(new Error('403')),
      drafts: () => Promise.resolve([]),
      launches: () => Promise.resolve([]),
    });
    const results = await Promise.allSettled(sources.map((s) => s.load()));
    expect(results.map((r) => r.status)).toEqual([
      'fulfilled',
      'rejected',
      'fulfilled',
      'fulfilled',
    ]);
  });
});

/** A `searchindex.js` the way mdBook writes one: the JSON escaped as a single-quoted JS string. */
const searchIndex = (index: unknown) =>
  `window.search = Object.assign(window.search, JSON.parse('${JSON.stringify(index)
    .replace(/\\/g, '\\\\')
    .replace(/'/g, "\\'")}'));\n`;

const BOOK = {
  doc_urls: ['introduction.html#crucible', "playbooks.html#it's-a-pack", 'outputs.html#paths'],
  index: {
    documentStore: {
      docs: {
        '0': { id: '0', title: 'crucible', breadcrumbs: 'Introduction » crucible', body: '' },
        '1': {
          id: '1',
          title: "It's a pack",
          breadcrumbs: "Playbooks » It's a pack",
          body: 'a \\ path and "quotes"',
        },
        '2': {
          id: '2',
          title: 'C:\\paths',
          breadcrumbs: 'Outputs » C:\\paths',
          body: 'line\nbreak',
        },
        '3': { id: '3', title: '', breadcrumbs: 'untitled', body: '' },
        '9': { id: '9', title: 'orphan', breadcrumbs: 'no url', body: '' },
      },
    },
  },
};

describe('docs', () => {
  it('read headings out of an mdBook search index, unescaping the JS string', () => {
    expect(docEntries(searchIndex(BOOK))).toEqual([
      {
        title: 'crucible',
        breadcrumbs: 'Introduction » crucible',
        url: 'introduction.html#crucible',
      },
      {
        title: "It's a pack",
        breadcrumbs: "Playbooks » It's a pack",
        url: "playbooks.html#it's-a-pack",
      },
      { title: 'C:\\paths', breadcrumbs: 'Outputs » C:\\paths', url: 'outputs.html#paths' },
    ]);
  });

  it('return nothing for a script that is not a search index', () => {
    for (const script of [
      '',
      'window.search = {};',
      "JSON.parse('not json'));",
      searchIndex({ doc_urls: 'x' }),
      searchIndex([1, 2]),
    ]) {
      expect(docEntries(script)).toEqual([]);
    }
  });

  it("link each heading under the book's base", () => {
    const [first] = docCommands(
      docEntries(searchIndex(BOOK)),
      'https://docs.example.com/crucible/'
    );
    expect(first?.title).toBe('crucible');
    expect(first?.subtitle).toBe('Introduction » crucible');
    expect(first?.action).toEqual({
      kind: 'external',
      url: 'https://docs.example.com/crucible/introduction.html#crucible',
    });
  });

  it('fetch the index once, and again after a failure', async () => {
    const asked: string[] = [];
    let fail = true;
    const source = docsSource((url) => {
      asked.push(url);
      return fail ? Promise.reject(new Error('offline')) : Promise.resolve(searchIndex(BOOK));
    }, 'https://docs.example.com/crucible/');
    await expect(source.load()).rejects.toThrow('offline');
    fail = false;
    expect(await source.load()).toHaveLength(3);
    expect(await source.load()).toHaveLength(3);
    expect(asked).toEqual([
      'https://docs.example.com/crucible/searchindex.js',
      'https://docs.example.com/crucible/searchindex.js',
    ]);
  });
});
