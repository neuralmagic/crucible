import type { languages } from 'monaco-editor/editor';

/// CEL, the webhook transform language, which Monaco ships no grammar for: literals, the keywords,
/// operators, strings (single, double, triple, raw, and bytes), and member calls.
export const CEL_TOKENS: languages.IMonarchLanguage = {
  defaultToken: '',
  tokenPostfix: '.cel',
  keywords: ['true', 'false', 'null', 'in'],
  tokenizer: {
    root: [
      [/\/\/.*$/, 'comment'],
      [/[rR]?[bB]?"""/, { token: 'string', next: '@tripleDouble' }],
      [/[rR]?[bB]?'''/, { token: 'string', next: '@tripleSingle' }],
      [/[rR]?[bB]?"/, { token: 'string', next: '@double' }],
      [/[rR]?[bB]?'/, { token: 'string', next: '@single' }],
      [/\d+\.\d*([eE][+-]?\d+)?|\.\d+([eE][+-]?\d+)?|\d+[eE][+-]?\d+/, 'number.float'],
      [/0x[0-9a-fA-F]+u?|\d+u?/, 'number'],
      [/[A-Za-z_][\w]*(?=\s*\()/, 'function'],
      [/[A-Za-z_][\w]*/, { cases: { '@keywords': 'keyword', '@default': 'identifier' } }],
      [/&&|\|\||==|!=|<=|>=|[<>!+\-*/%?:]/, 'operator'],
      [/[()[\]{}.,]/, 'delimiter'],
    ],
    double: [
      [/[^\\"]+/, 'string'],
      [/\\./, 'string.escape'],
      [/"/, { token: 'string', next: '@pop' }],
    ],
    single: [
      [/[^\\']+/, 'string'],
      [/\\./, 'string.escape'],
      [/'/, { token: 'string', next: '@pop' }],
    ],
    tripleDouble: [
      [/"""/, { token: 'string', next: '@pop' }],
      [/./, 'string'],
    ],
    tripleSingle: [
      [/'''/, { token: 'string', next: '@pop' }],
      [/./, 'string'],
    ],
  },
};

export const CEL_CONFIGURATION: languages.LanguageConfiguration = {
  comments: { lineComment: '//' },
  brackets: [
    ['(', ')'],
    ['[', ']'],
    ['{', '}'],
  ],
  autoClosingPairs: [
    { open: '(', close: ')' },
    { open: '[', close: ']' },
    { open: '{', close: '}' },
    { open: '"', close: '"', notIn: ['string'] },
    { open: "'", close: "'", notIn: ['string'] },
  ],
};
