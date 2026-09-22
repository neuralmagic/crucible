import type { languages } from 'monaco-editor/editor';

/// TOML, which Monaco ships no grammar for. Enough of the spec for a pack manifest: tables and
/// array-of-table headers, bare and quoted keys, basic and literal strings (single and multiline),
/// numbers, booleans, and dates.
export const TOML_TOKENS: languages.IMonarchLanguage = {
  defaultToken: '',
  tokenPostfix: '.toml',
  tokenizer: {
    root: [
      [/^\s*\[\[.*?\]\]/, 'metatag'],
      [/^\s*\[.*?\]/, 'metatag'],
      [/#.*$/, 'comment'],
      [/[A-Za-z0-9_-]+(?=\s*(\.\s*[A-Za-z0-9_-]+\s*)*=)/, 'key'],
      [/"""/, { token: 'string', next: '@blockString' }],
      [/'''/, { token: 'string', next: '@blockLiteral' }],
      [/"/, { token: 'string', next: '@string' }],
      [/'/, { token: 'string', next: '@literal' }],
      [/\b(true|false)\b/, 'keyword'],
      [/\b\d{4}-\d{2}-\d{2}([Tt ]\d{2}:\d{2}:\d{2}(\.\d+)?([Zz]|[+-]\d{2}:\d{2})?)?/, 'number'],
      [/[+-]?(\d[\d_]*)?\.\d[\d_]*([eE][+-]?\d+)?/, 'number.float'],
      [/[+-]?0x[0-9a-fA-F_]+|[+-]?0o[0-7_]+|[+-]?0b[01_]+/, 'number.hex'],
      [/[+-]?\d[\d_]*([eE][+-]?\d+)?/, 'number'],
      [/[=,[\]{}]/, 'delimiter'],
    ],
    string: [
      [/[^\\"]+/, 'string'],
      [/\\./, 'string.escape'],
      [/"/, { token: 'string', next: '@pop' }],
    ],
    literal: [
      [/[^']+/, 'string'],
      [/'/, { token: 'string', next: '@pop' }],
    ],
    blockString: [
      [/[^\\"]+/, 'string'],
      [/\\./, 'string.escape'],
      [/"""/, { token: 'string', next: '@pop' }],
      [/"/, 'string'],
    ],
    blockLiteral: [
      [/[^']+/, 'string'],
      [/'''/, { token: 'string', next: '@pop' }],
      [/'/, 'string'],
    ],
  },
};
