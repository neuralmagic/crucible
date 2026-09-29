import type { languages } from 'monaco-editor/editor';

export const CEDAR_KEYWORDS = [
  'permit',
  'forbid',
  'when',
  'unless',
  'principal',
  'action',
  'resource',
  'context',
  'in',
  'has',
  'like',
  'is',
  'if',
  'then',
  'else',
] as const;

export const CEDAR_TOKENS: languages.IMonarchLanguage = {
  defaultToken: '',
  tokenPostfix: '.cedar',
  keywords: CEDAR_KEYWORDS,
  brackets: [
    { open: '{', close: '}', token: 'delimiter.curly' },
    { open: '[', close: ']', token: 'delimiter.square' },
    { open: '(', close: ')', token: 'delimiter.parenthesis' },
  ],
  tokenizer: {
    root: [
      [/\/\/.*$/, 'comment'],
      [/@[A-Za-z_]\w*/, { token: 'metatag', next: '@annotation' }],
      [/(\bis)(\s+)((?:[A-Za-z_]\w*\s*::\s*)*[A-Za-z_]\w*)/, ['keyword', '', 'type.identifier']],
      [/[A-Za-z_]\w*(?=\s*::)/, 'type.identifier'],
      [/::/, 'delimiter'],
      [/\b(?:true|false)\b/, 'constant'],
      [/[A-Za-z_]\w*/, { cases: { '@keywords': 'keyword', '@default': 'identifier' } }],
      [/"/, { token: 'string', next: '@string' }],
      [/\d+/, 'number'],
      [/==|!=|<=|>=|&&|\|\||[<>!+\-*]/, 'operator'],
      [/[.,;]/, 'delimiter'],
      [/[{}[\]()]/, '@brackets'],
    ],
    annotation: [
      [/\s+/, ''],
      [/\(/, 'metatag'],
      [/"(?:[^"\\]|\\.)*"/, 'metatag'],
      [/\)/, { token: 'metatag', next: '@pop' }],
      [/./, { token: '@rematch', next: '@pop' }],
    ],
    string: [
      [/[^\\"]+/, 'string'],
      [/\\./, 'string.escape'],
      [/"/, { token: 'string', next: '@pop' }],
    ],
  },
};

export const CEDAR_CONFIGURATION: languages.LanguageConfiguration = {
  comments: { lineComment: '//' },
  brackets: [
    ['{', '}'],
    ['[', ']'],
    ['(', ')'],
  ],
  autoClosingPairs: [
    { open: '{', close: '}' },
    { open: '[', close: ']' },
    { open: '(', close: ')' },
    { open: '"', close: '"', notIn: ['string', 'comment'] },
  ],
  surroundingPairs: [
    { open: '{', close: '}' },
    { open: '[', close: ']' },
    { open: '(', close: ')' },
    { open: '"', close: '"' },
  ],
};
