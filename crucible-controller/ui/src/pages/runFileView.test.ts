import { describe, expect, it } from 'vitest';
import type { components } from '../api/schema';
import {
  decodeFileKey,
  downloadName,
  encodeFileKey,
  imageMimeType,
  inlineKind,
  parseRunFilePath,
  prettyJson,
  producerOf,
  runFileApiPath,
  runFilePath,
  runFileUrl,
  runPath,
  selectRunFile,
  shouldAnchorFiles,
  textKind,
} from './runFileView';

type RunFile = components['schemas']['RunFile'];

function file(over: Partial<RunFile> = {}): RunFile {
  return {
    task: 'rollup',
    instance: null,
    key: 'rollup/REPORT.md',
    path: 'REPORT.md',
    size_bytes: 12,
    ...over,
  };
}

const KEYS = [
  'rollup/REPORT.md',
  'assess[30689]/VERDICT.json',
  'assess[a b]/notes on the run.txt',
  'triage/résumé — δοκιμή.md',
  'weird/100% done.log',
  'nested/deep/path/file.json',
  'plus+and&amp/q?x=1#frag.txt',
];

describe('file key encoding', () => {
  it('round-trips every shape of key', () => {
    for (const key of KEYS) {
      expect(decodeFileKey(encodeFileKey(key))).toBe(key);
    }
  });

  it('keeps slashes as separators and escapes everything else', () => {
    expect(encodeFileKey('assess[30689]/VERDICT.json')).toBe('assess%5B30689%5D/VERDICT.json');
    expect(encodeFileKey('a b/c d.txt')).toBe('a%20b/c%20d.txt');
    expect(encodeFileKey('weird/100% done.log')).toBe('weird/100%25%20done.log');
    expect(encodeFileKey('plus+and&amp/q?x=1#frag.txt')).toBe(
      'plus%2Band%26amp/q%3Fx%3D1%23frag.txt',
    );
  });

  it('leaves a malformed escape alone instead of throwing', () => {
    expect(decodeFileKey('bad/%zz')).toBe('bad/%zz');
  });
});

describe('runFilePath', () => {
  it('addresses a bare run', () => {
    expect(runFilePath({ launchKey: null, runId: 'RUN-0412', fileKey: 'rollup/REPORT.md' })).toBe(
      '/runs/RUN-0412/files/rollup/REPORT.md',
    );
  });

  it('keeps the launch context when the run hangs under one', () => {
    expect(
      runFilePath({
        launchKey: 'playbook:survey:0199',
        runId: 'playbook_0199',
        fileKey: 'assess[30689]/VERDICT.json',
      }),
    ).toBe('/playbook-runs/playbook%3Asurvey%3A0199/runs/playbook_0199/files/assess%5B30689%5D/VERDICT.json');
  });

  it('builds the run page path for both shapes', () => {
    expect(runPath('RUN-0412', null)).toBe('/runs/RUN-0412');
    expect(runPath('r1', 'playbook:survey:0199')).toBe('/playbook-runs/playbook%3Asurvey%3A0199/runs/r1');
  });

  it('prefixes the origin for a shareable link', () => {
    expect(
      runFileUrl('https://crucible.example.com', {
        launchKey: null,
        runId: 'RUN-0412',
        fileKey: 'rollup/REPORT.md',
      }),
    ).toBe('https://crucible.example.com/runs/RUN-0412/files/rollup/REPORT.md');
  });

  it('points the download at the authenticated bytes endpoint', () => {
    expect(runFileApiPath('RUN-0412', 'assess[3]/VERDICT.json')).toBe(
      '/api/runs/RUN-0412/files/assess%5B3%5D/VERDICT.json',
    );
  });
});

describe('parseRunFilePath', () => {
  it('reads back every key a link can carry', () => {
    for (const key of KEYS) {
      const route = { launchKey: null, runId: 'RUN-0412', fileKey: key };
      expect(parseRunFilePath(runFilePath(route))).toEqual(route);
    }
  });

  it('reads back keys under a launch', () => {
    for (const key of KEYS) {
      const route = { launchKey: 'playbook:survey:0199', runId: 'playbook_0199', fileKey: key };
      expect(parseRunFilePath(runFilePath(route))).toEqual(route);
    }
  });

  it('decodes a run id with odd characters', () => {
    expect(parseRunFilePath('/runs/run%20one/files/a.md')).toEqual({
      launchKey: null,
      runId: 'run one',
      fileKey: 'a.md',
    });
  });

  it('is null for anything that is not a file deep link', () => {
    expect(parseRunFilePath('/runs/RUN-0412')).toBeNull();
    expect(parseRunFilePath('/runs/RUN-0412/files')).toBeNull();
    expect(parseRunFilePath('/runs/RUN-0412/files/')).toBeNull();
    expect(parseRunFilePath('/playbook-runs/k/runs/r')).toBeNull();
    expect(parseRunFilePath('/playbook-runs')).toBeNull();
    expect(parseRunFilePath('/issues/ABC-1')).toBeNull();
    expect(parseRunFilePath('/')).toBeNull();
  });
});

describe('inlineKind', () => {
  it('maps extensions to renderers', () => {
    expect(inlineKind('REPORT.md')).toBe('markdown');
    expect(inlineKind('VERDICT.JSON')).toBe('json');
    expect(inlineKind('out.txt')).toBe('text');
    expect(inlineKind('agent.log')).toBe('text');
    expect(inlineKind('chart.png')).toBe('image');
    expect(inlineKind('photo.JPEG')).toBe('image');
    expect(inlineKind('diagram.svg')).toBe('image');
    expect(inlineKind('bundle.tar.gz')).toBeNull();
    expect(inlineKind('Makefile')).toBeNull();
    expect(inlineKind('dir.d/README')).toBeNull();
    expect(inlineKind('.gitignore')).toBeNull();
  });

  it('names an image media type so the bytes render as themselves', () => {
    expect(imageMimeType('a/b.png')).toBe('image/png');
    expect(imageMimeType('a/b.jpg')).toBe('image/jpeg');
    expect(imageMimeType('a/b.md')).toBeNull();
  });
});

describe('selectRunFile', () => {
  const entries = [
    file({ key: 'assess[1]/VERDICT.json', path: 'VERDICT.json', task: 'assess', instance: '1' }),
    file({ key: 'rollup/REPORT.md', path: 'REPORT.md' }),
  ];

  it('resolves the requested key', () => {
    expect(selectRunFile(entries, 'assess[1]/VERDICT.json')).toEqual({
      state: 'found',
      file: entries[0],
    });
  });

  it('reports a key the run never captured rather than showing another file', () => {
    expect(selectRunFile(entries, 'rollup/GONE.md')).toEqual({
      state: 'missing',
      key: 'rollup/GONE.md',
    });
  });

  it('falls back to the report when nothing was requested', () => {
    expect(selectRunFile(entries, null)).toEqual({ state: 'found', file: entries[1] });
  });

  it('falls back to any markdown, then to the first entry', () => {
    const md = file({ key: 'a/NOTES.md', path: 'NOTES.md' });
    const json = file({ key: 'a/x.json', path: 'x.json' });
    expect(selectRunFile([json, md], null)).toEqual({ state: 'found', file: md });
    expect(selectRunFile([json], null)).toEqual({ state: 'found', file: json });
  });

  it('is empty when the run captured nothing', () => {
    expect(selectRunFile([], null)).toEqual({ state: 'empty' });
    expect(selectRunFile([], 'rollup/REPORT.md')).toEqual({
      state: 'missing',
      key: 'rollup/REPORT.md',
    });
  });
});

describe('producerOf', () => {
  it('brackets the fan-out instance', () => {
    expect(producerOf(file({ task: 'assess', instance: '30689' }))).toBe('assess[30689]');
    expect(producerOf(file({ task: 'rollup', instance: null }))).toBe('rollup');
  });
});

describe('prettyJson', () => {
  it('indents valid json and leaves anything else untouched', () => {
    expect(prettyJson('{"a":1}')).toBe('{\n  "a": 1\n}');
    expect(prettyJson('not json')).toBe('not json');
  });
});

describe('downloadName', () => {
  it('flattens the key so the saved file is one name', () => {
    expect(downloadName('RUN-0412', 'assess[1]/VERDICT.json')).toBe(
      'RUN-0412-assess[1]-VERDICT.json',
    );
  });
});

describe('textKind', () => {
  it('drops images so a text surface never gets bytes', () => {
    expect(textKind('a.md')).toBe('markdown');
    expect(textKind('a.png')).toBeNull();
    expect(textKind('a.bin')).toBeNull();
  });
});

describe('shouldAnchorFiles', () => {
  it('anchors a page opened on a deep link once its listing has loaded', () => {
    expect(shouldAnchorFiles({ openedOnDeepLink: true, anchored: false, loaded: 0 })).toBe(false);
    expect(shouldAnchorFiles({ openedOnDeepLink: true, anchored: false, loaded: 3 })).toBe(true);
    expect(shouldAnchorFiles({ openedOnDeepLink: true, anchored: true, loaded: 3 })).toBe(false);
  });

  it('never anchors a page opened without a file key, whatever the click rewrote the URL to', () => {
    expect(shouldAnchorFiles({ openedOnDeepLink: false, anchored: false, loaded: 3 })).toBe(false);
  });
});
