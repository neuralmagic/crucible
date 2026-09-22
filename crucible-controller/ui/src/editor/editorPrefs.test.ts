import { describe, expect, it } from 'vitest';
import {
  EDITOR_PREFS_DEFAULTS,
  monacoThemeName,
  parseEditorPrefs,
  surfaceOptions,
} from './editorPrefs';

describe('parseEditorPrefs', () => {
  it('defaults an absent document', () => {
    expect(parseEditorPrefs(undefined)).toEqual(EDITOR_PREFS_DEFAULTS);
    expect(parseEditorPrefs({})).toEqual(EDITOR_PREFS_DEFAULTS);
  });

  it('takes every stored override', () => {
    expect(
      parseEditorPrefs({
        theme: 'contrast',
        fontSize: 15,
        wordWrap: true,
        minimap: true,
        whitespace: true,
      })
    ).toEqual({
      theme: 'contrast',
      fontSize: 15,
      wordWrap: true,
      minimap: true,
      whitespace: true,
    });
  });

  it('refuses a value the client does not own', () => {
    const parsed = parseEditorPrefs({ theme: 'solarized', wordWrap: 'yes', minimap: 1 });
    expect(parsed.theme).toBe(EDITOR_PREFS_DEFAULTS.theme);
    expect(parsed.wordWrap).toBe(EDITOR_PREFS_DEFAULTS.wordWrap);
    expect(parsed.minimap).toBe(EDITOR_PREFS_DEFAULTS.minimap);
  });

  it('clamps a font size into a readable range', () => {
    expect(parseEditorPrefs({ fontSize: 400 }).fontSize).toBe(24);
    expect(parseEditorPrefs({ fontSize: 1 }).fontSize).toBe(9);
    expect(parseEditorPrefs({ fontSize: 12.6 }).fontSize).toBe(13);
    expect(parseEditorPrefs({ fontSize: Number.NaN }).fontSize).toBe(
      EDITOR_PREFS_DEFAULTS.fontSize
    );
  });
});

describe('monacoThemeName', () => {
  it('follows the app theme within one pairing', () => {
    expect(monacoThemeName('paper', 'light')).toBe('crucible-paper');
    expect(monacoThemeName('paper', 'dark')).toBe('crucible-ink');
    expect(monacoThemeName('classic', 'dark')).toBe('vs-dark');
    expect(monacoThemeName('contrast', 'light')).toBe('hc-light');
  });
});

describe('surfaceOptions', () => {
  it('resolves the document into what every surface is configured with', () => {
    const options = surfaceOptions({ ...EDITOR_PREFS_DEFAULTS, wordWrap: true, whitespace: true });
    expect(options).toMatchObject({
      fontSize: 12,
      lineHeight: 19,
      wordWrap: 'on',
      minimap: { enabled: false },
      renderWhitespace: 'all',
    });
  });

  /// The chrome the app expects rides on every surface, not just the studio's.
  it('carries the app chrome whatever the document says', () => {
    const options = surfaceOptions(EDITOR_PREFS_DEFAULTS);
    expect(options.roundedSelection).toBe(false);
    expect(options.glyphMargin).toBe(false);
    expect(options.overviewRulerBorder).toBe(false);
    expect(options.scrollbar?.useShadows).toBe(false);
    expect(options.fontFamily).toContain('Ioskeley Mono');
  });
});
