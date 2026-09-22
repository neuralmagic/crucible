/// The app's palette as Monaco themes. Every colour Monaco paints comes from a token in
/// `global.css`; `theme.test.ts` reads that file and fails when the two drift apart.

import type * as monaco from 'monaco-editor/editor';

export type AppTheme = 'dark' | 'light';

export interface Palette {
  paper: string;
  surface: string;
  sunk: string;
  ink: string;
  ink2: string;
  ink3: string;
  rule: string;
  ruleHard: string;
  green: string;
  amber: string;
  red: string;
  blue: string;
  hi: string;
}

export const PALETTE_KEYS = [
  'paper',
  'surface',
  'sunk',
  'ink',
  'ink2',
  'ink3',
  'rule',
  'ruleHard',
  'green',
  'amber',
  'red',
  'blue',
  'hi',
] as const satisfies readonly (keyof Palette)[];

/// Each palette key and the custom property it has to equal.
export const TOKEN_OF: Record<keyof Palette, string> = {
  paper: '--color-paper',
  surface: '--color-surface',
  sunk: '--color-sunk',
  ink: '--color-ink',
  ink2: '--color-ink-2',
  ink3: '--color-ink-3',
  rule: '--color-rule',
  ruleHard: '--color-rule-hard',
  green: '--color-green',
  amber: '--color-amber',
  red: '--color-red',
  blue: '--color-blue',
  hi: '--color-hi',
};

export const PALETTES: Record<AppTheme, Palette> = {
  light: {
    paper: '#edebe2',
    surface: '#ffffff',
    sunk: '#dddacd',
    ink: '#100f0e',
    ink2: '#464439',
    ink3: '#615f56',
    rule: '#c4c0b0',
    ruleHard: '#8a8676',
    green: '#00663a',
    amber: '#7e4f00',
    red: '#96181f',
    blue: '#1a4785',
    hi: '#ffeda8',
  },
  dark: {
    paper: '#0a0a09',
    surface: '#1c1c19',
    sunk: '#040403',
    ink: '#f5f3ea',
    ink2: '#adaa9d',
    ink3: '#878477',
    rule: '#3a3830',
    ruleHard: '#666254',
    green: '#5acb86',
    amber: '#dfa53f',
    red: '#e56b69',
    blue: '#83afe8',
    hi: '#463d18',
  },
};

const CLEAR = '#00000000';

function fade(color: string, percent: number): string {
  const byte = Math.round((percent / 100) * 255);
  return `${color}${byte.toString(16).padStart(2, '0')}`;
}

/// Monaco wants its rule colours without the leading hash.
function bare(color: string): string {
  return color.slice(1);
}

function rules(p: Palette): monaco.editor.ITokenThemeRule[] {
  return [
    { token: '', foreground: bare(p.ink) },
    { token: 'comment', foreground: bare(p.ink3), fontStyle: 'italic' },
    { token: 'string', foreground: bare(p.green) },
    { token: 'regexp', foreground: bare(p.amber) },
    { token: 'number', foreground: bare(p.amber) },
    { token: 'constant', foreground: bare(p.amber) },
    { token: 'keyword', foreground: bare(p.red) },
    { token: 'operator', foreground: bare(p.ink2) },
    { token: 'delimiter', foreground: bare(p.ink2) },
    { token: 'identifier', foreground: bare(p.ink) },
    { token: 'type', foreground: bare(p.blue) },
    { token: 'type.identifier', foreground: bare(p.blue) },
    { token: 'tag', foreground: bare(p.blue) },
    { token: 'metatag', foreground: bare(p.blue) },
    { token: 'key', foreground: bare(p.blue) },
    { token: 'attribute.name', foreground: bare(p.blue) },
    { token: 'attribute.value', foreground: bare(p.green) },
    { token: 'string.key.json', foreground: bare(p.blue) },
    { token: 'string.value.json', foreground: bare(p.green) },
    { token: 'variable', foreground: bare(p.ink) },
    { token: 'strong', foreground: bare(p.ink), fontStyle: 'bold' },
    { token: 'emphasis', foreground: bare(p.ink), fontStyle: 'italic' },
    { token: 'invalid', foreground: bare(p.red) },
  ];
}

function colors(p: Palette): Record<string, string> {
  return {
    focusBorder: p.blue,
    foreground: p.ink,
    'widget.shadow': CLEAR,
    'widget.border': p.ruleHard,
    'selection.background': p.hi,
    'icon.foreground': p.ink2,
    'textLink.foreground': p.blue,
    'textLink.activeForeground': p.blue,

    'editor.background': p.paper,
    'editor.foreground': p.ink,
    'editorCursor.foreground': p.ink,
    'editor.lineHighlightBackground': fade(p.hi, 35),
    'editor.lineHighlightBorder': CLEAR,
    'editor.selectionBackground': p.hi,
    'editor.inactiveSelectionBackground': fade(p.hi, 45),
    'editor.selectionHighlightBackground': fade(p.hi, 45),
    'editor.selectionHighlightBorder': CLEAR,
    'editor.wordHighlightBackground': fade(p.hi, 35),
    'editor.wordHighlightStrongBackground': fade(p.hi, 45),
    'editor.findMatchBackground': p.hi,
    'editor.findMatchHighlightBackground': fade(p.hi, 45),
    'editor.findMatchBorder': p.ruleHard,
    'editor.rangeHighlightBackground': fade(p.hi, 25),
    'editor.foldBackground': fade(p.sunk, 60),
    'editor.hoverHighlightBackground': fade(p.hi, 25),
    'editorWhitespace.foreground': p.rule,
    'editorIndentGuide.background1': p.rule,
    'editorIndentGuide.activeBackground1': p.ruleHard,
    'editorLineNumber.foreground': p.ink3,
    'editorLineNumber.activeForeground': p.ink,
    'editorGutter.background': p.paper,
    'editorGutter.modifiedBackground': p.blue,
    'editorGutter.addedBackground': p.green,
    'editorGutter.deletedBackground': p.red,
    'editorBracketHighlight.foreground1': p.ink2,
    'editorBracketHighlight.foreground2': p.ink2,
    'editorBracketHighlight.foreground3': p.ink2,
    'editorBracketHighlight.foreground4': p.ink2,
    'editorBracketHighlight.foreground5': p.ink2,
    'editorBracketHighlight.foreground6': p.ink2,
    'editorBracketHighlight.unexpectedBracket.foreground': p.red,
    'editorBracketMatch.background': CLEAR,
    'editorBracketMatch.border': p.ruleHard,
    'editorRuler.foreground': p.rule,
    'editorLink.activeForeground': p.blue,
    'editorError.foreground': p.red,
    'editorWarning.foreground': p.amber,
    'editorInfo.foreground': p.blue,
    'editorOverviewRuler.border': CLEAR,
    'editorOverviewRuler.background': p.paper,
    'editorOverviewRuler.errorForeground': p.red,
    'editorOverviewRuler.warningForeground': p.amber,
    'editorOverviewRuler.infoForeground': p.blue,
    'editorOverviewRuler.addedForeground': p.green,
    'editorOverviewRuler.deletedForeground': p.red,
    'editorOverviewRuler.modifiedForeground': p.blue,

    'scrollbar.shadow': CLEAR,
    'scrollbarSlider.background': fade(p.ruleHard, 40),
    'scrollbarSlider.hoverBackground': fade(p.ruleHard, 65),
    'scrollbarSlider.activeBackground': p.ruleHard,

    'editorWidget.background': p.surface,
    'editorWidget.foreground': p.ink,
    'editorWidget.border': p.ruleHard,
    'editorHoverWidget.background': p.surface,
    'editorHoverWidget.foreground': p.ink,
    'editorHoverWidget.border': p.ruleHard,
    'editorSuggestWidget.background': p.surface,
    'editorSuggestWidget.foreground': p.ink,
    'editorSuggestWidget.border': p.ruleHard,
    'editorSuggestWidget.selectedBackground': p.hi,
    'editorSuggestWidget.selectedForeground': p.ink,
    'editorSuggestWidget.highlightForeground': p.blue,
    'editorMarkerNavigation.background': p.surface,
    'editorMarkerNavigationError.background': p.red,
    'editorMarkerNavigationWarning.background': p.amber,
    'editorMarkerNavigationInfo.background': p.blue,
    'peekViewEditor.background': p.paper,
    'peekViewResult.background': p.surface,
    'peekViewTitle.background': p.surface,
    'peekView.border': p.ruleHard,

    'input.background': p.paper,
    'input.foreground': p.ink,
    'input.border': p.ruleHard,
    'input.placeholderForeground': p.ink3,
    'inputOption.activeBorder': p.blue,
    'dropdown.background': p.surface,
    'dropdown.foreground': p.ink,
    'dropdown.border': p.ruleHard,
    'list.hoverBackground': p.sunk,
    'list.hoverForeground': p.ink,
    'list.focusBackground': p.hi,
    'list.focusForeground': p.ink,
    'list.activeSelectionBackground': p.hi,
    'list.activeSelectionForeground': p.ink,
    'list.inactiveSelectionBackground': fade(p.hi, 45),
    'list.highlightForeground': p.blue,
    'menu.background': p.surface,
    'menu.foreground': p.ink,
    'menu.border': p.ruleHard,
    'menu.selectionBackground': p.hi,
    'menu.selectionForeground': p.ink,

    'diffEditor.border': p.ruleHard,
    'diffEditor.diagonalFill': p.rule,
    'diffEditor.insertedTextBackground': fade(p.green, 14),
    'diffEditor.removedTextBackground': fade(p.red, 14),
    'diffEditor.insertedLineBackground': fade(p.green, 8),
    'diffEditor.removedLineBackground': fade(p.red, 8),
    'diffEditorGutter.insertedLineBackground': fade(p.green, 12),
    'diffEditorGutter.removedLineBackground': fade(p.red, 12),
    'diffEditorOverview.insertedForeground': fade(p.green, 60),
    'diffEditorOverview.removedForeground': fade(p.red, 60),
    'diffEditor.unchangedRegionBackground': p.paper,
    'diffEditor.unchangedRegionForeground': p.ink3,
    'diffEditor.unchangedCodeBackground': CLEAR,
  };
}

/// The Monaco theme names this app defines, one per app theme.
export const THEME_NAMES: Record<AppTheme, string> = {
  light: 'crucible-paper',
  dark: 'crucible-ink',
};

export function monacoTheme(app: AppTheme): monaco.editor.IStandaloneThemeData {
  const palette = PALETTES[app];
  return {
    base: app === 'light' ? 'vs' : 'vs-dark',
    inherit: true,
    rules: rules(palette),
    colors: colors(palette),
  };
}
