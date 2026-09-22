/// The one place Monaco is loaded, configured and taught what this app's files are. Imported for
/// its side effects by the surfaces; everything below runs once per page.
///
/// The bundle is assembled here rather than pulled from a CDN: `monaco-editor/editor` is the core,
/// the feature and language registrations are opted into one by one, and the workers are real Vite
/// worker modules, so the editor works offline and behind the proxy like the rest of the SPA.

import * as monaco from 'monaco-editor/editor';
import { loader } from '@monaco-editor/react';
import 'monaco-editor/features/register.all';
import 'monaco-editor/languages/definitions/python/register';
import 'monaco-editor/languages/definitions/markdown/register';
import 'monaco-editor/languages/definitions/shell/register';
import 'monaco-editor/languages/definitions/yaml/register';
import 'monaco-editor/languages/features/json/register';
import EditorWorker from 'monaco-editor/editor/editor.worker.js?worker';
import JsonWorker from 'monaco-editor/languages/features/json/json.worker.js?worker';
import { monacoTheme, THEME_NAMES } from './theme';
import { TOML_TOKENS } from './toml';

declare global {
  interface Window {
    MonacoEnvironment?: monaco.Environment;
  }
}

self.MonacoEnvironment = {
  getWorker(_workerId: string, label: string) {
    return label === 'json' ? new JsonWorker() : new EditorWorker();
  },
};

/// Starlark rides the `python` grammar (`languageOf`), so the only grammar this app has to supply
/// is TOML, which Monaco does not ship.
monaco.languages.register({ id: 'toml', extensions: ['.toml'], aliases: ['TOML', 'toml'] });
monaco.languages.setLanguageConfiguration('toml', {
  comments: { lineComment: '#' },
  brackets: [
    ['[', ']'],
    ['{', '}'],
  ],
  autoClosingPairs: [
    { open: '[', close: ']' },
    { open: '{', close: '}' },
    { open: '"', close: '"' },
    { open: "'", close: "'" },
  ],
});
monaco.languages.setMonarchTokensProvider('toml', TOML_TOKENS);

for (const app of ['light', 'dark'] as const) {
  monaco.editor.defineTheme(THEME_NAMES[app], monacoTheme(app));
}

loader.config({ monaco });

export { monaco };
