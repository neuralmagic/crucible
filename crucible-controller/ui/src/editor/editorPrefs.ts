/// The one editor-preferences document. It is stored server-side against the caller's identity
/// (`GET`/`PUT /api/prefs/editor`), so a fresh browser reads back what was set; the server keeps
/// the blob opaque, which makes parsing and clamping this module's job.

import type * as monaco from 'monaco-editor/editor';
import { type AppTheme, THEME_NAMES } from './theme';

export const HIGHLIGHT_THEMES = ['paper', 'classic', 'contrast'] as const;
export type HighlightTheme = (typeof HIGHLIGHT_THEMES)[number];

export interface EditorPrefs {
  /// A pairing, not a single theme: each one names a light and a dark member, and which is used
  /// follows the app theme.
  theme: HighlightTheme;
  fontSize: number;
  wordWrap: boolean;
  minimap: boolean;
  whitespace: boolean;
}

export const EDITOR_PREFS_DEFAULTS: EditorPrefs = {
  theme: 'paper',
  fontSize: 12,
  wordWrap: false,
  minimap: false,
  whitespace: false,
};

export const FONT_SIZES = [11, 12, 13, 15] as const;

const MIN_FONT = 9;
const MAX_FONT = 24;

function isHighlightTheme(value: unknown): value is HighlightTheme {
  return HIGHLIGHT_THEMES.some((theme) => theme === value);
}

export function parseEditorPrefs(stored: Record<string, unknown> | undefined): EditorPrefs {
  const raw = stored ?? {};
  const size = raw.fontSize;
  return {
    theme: isHighlightTheme(raw.theme) ? raw.theme : EDITOR_PREFS_DEFAULTS.theme,
    fontSize:
      typeof size === 'number' && Number.isFinite(size)
        ? Math.min(MAX_FONT, Math.max(MIN_FONT, Math.round(size)))
        : EDITOR_PREFS_DEFAULTS.fontSize,
    wordWrap: typeof raw.wordWrap === 'boolean' ? raw.wordWrap : EDITOR_PREFS_DEFAULTS.wordWrap,
    minimap: typeof raw.minimap === 'boolean' ? raw.minimap : EDITOR_PREFS_DEFAULTS.minimap,
    whitespace:
      typeof raw.whitespace === 'boolean' ? raw.whitespace : EDITOR_PREFS_DEFAULTS.whitespace,
  };
}

/// The two members of one curated pairing. `paper` is the app's own palette; the other two are
/// Monaco's shipped themes, kept so a reader who wants a familiar editor gets one.
const PAIRINGS: Record<HighlightTheme, Record<AppTheme, string>> = {
  paper: THEME_NAMES,
  classic: { light: 'vs', dark: 'vs-dark' },
  contrast: { light: 'hc-light', dark: 'hc-black' },
};

export function monacoThemeName(theme: HighlightTheme, app: AppTheme): string {
  return PAIRINGS[theme][app];
}

/// The Monaco options one preferences document resolves to, applied to every surface. Everything
/// past the stored prefs is the chrome the app expects: no radius, no shadows, no minimap ruler,
/// the app's mono face at the app's rhythm.
export type SurfaceOptions = monaco.editor.IEditorOptions;

const MONO = "'Ioskeley Mono', ui-monospace, SFMono-Regular, Menlo, monospace";

/// The app's body line height, applied to code so a surface sits on the same rhythm as the text
/// around it.
const LINE_HEIGHT = 1.55;

export function surfaceOptions(prefs: EditorPrefs): SurfaceOptions {
  return {
    fontSize: prefs.fontSize,
    lineHeight: Math.round(prefs.fontSize * LINE_HEIGHT),
    fontFamily: MONO,
    fontWeight: '400',
    fontLigatures: false,
    wordWrap: prefs.wordWrap ? 'on' : 'off',
    minimap: { enabled: prefs.minimap, renderCharacters: false, showSlider: 'always' },
    renderWhitespace: prefs.whitespace ? 'all' : 'none',
    automaticLayout: true,
    scrollBeyondLastLine: false,
    roundedSelection: false,
    glyphMargin: false,
    folding: false,
    lineNumbersMinChars: 3,
    lineDecorationsWidth: 8,
    overviewRulerBorder: false,
    hideCursorInOverviewRuler: true,
    renderLineHighlightOnlyWhenFocus: true,
    occurrencesHighlight: 'off',
    selectionHighlight: false,
    matchBrackets: 'near',
    guides: { indentation: true, highlightActiveIndentation: false, bracketPairs: false },
    padding: { top: 6, bottom: 6 },
    stickyScroll: { enabled: false },
    scrollbar: {
      alwaysConsumeMouseWheel: false,
      useShadows: false,
      verticalScrollbarSize: 9,
      horizontalScrollbarSize: 9,
      verticalSliderSize: 9,
      horizontalSliderSize: 9,
    },
  };
}
