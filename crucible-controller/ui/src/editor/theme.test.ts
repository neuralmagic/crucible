import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import { APP_BRANDS } from '../appTheme';
import { monacoTheme, PALETTE_KEYS, PALETTES, THEME_NAMES, TOKEN_OF } from './theme';

function stylesheet(path: string): string {
  return readFileSync(fileURLToPath(new URL(path, import.meta.url)), 'utf8');
}

const GLOBAL_CSS = stylesheet('../global.css');
const REDHAT_CSS = stylesheet('../brands/redhat.css');

/// Every `--color-*` declared in one CSS block, keyed by custom property.
function block(css: string, open: string): Map<string, string> {
  const start = css.indexOf(open);
  expect(start, `${open} is declared`).toBeGreaterThan(-1);
  const body = css.slice(start + open.length, css.indexOf('}', start));
  const declared = new Map<string, string>();
  for (const line of body.split(';')) {
    const [name, value] = line.split(':');
    if (name === undefined || value === undefined) continue;
    const property = name.trim();
    if (property.startsWith('--color-')) declared.set(property, value.trim());
  }
  return declared;
}

const APP_THEMES = ['light', 'dark'] as const;

const BLOCKS = {
  crucible: {
    light: block(GLOBAL_CSS, '@theme {'),
    dark: block(GLOBAL_CSS, ":root[data-theme='dark'] {"),
  },
  redhat: {
    light: block(REDHAT_CSS, ":root[data-brand='redhat'] {"),
    dark: block(REDHAT_CSS, ":root[data-brand='redhat'][data-theme='dark'] {"),
  },
};

describe('the Monaco palettes', () => {
  for (const brand of APP_BRANDS) {
    for (const app of APP_THEMES) {
      it(`is the ${brand} ${app} tokens, to the byte`, () => {
        const declared = BLOCKS[brand][app];
        const palette = PALETTES[brand][app];
        for (const key of PALETTE_KEYS) {
          const token = TOKEN_OF[key];
          expect(palette[key], `${key} follows ${token}`).toBe(declared.get(token));
        }
      });
    }
  }

  it('defines one uniquely named theme per brand and app theme', () => {
    const names = APP_BRANDS.flatMap((brand) => APP_THEMES.map((app) => THEME_NAMES[brand][app]));
    expect(new Set(names).size).toBe(names.length);
  });

  it('paints no colour the app does not own', () => {
    for (const brand of APP_BRANDS) {
      for (const app of APP_THEMES) {
        const owned = new Set([...PALETTE_KEYS.map((key) => PALETTES[brand][app][key]), '#000000']);
        const theme = monacoTheme(brand, app);
        for (const [key, value] of Object.entries(theme.colors)) {
          // Anything faded keeps the token it was faded from.
          expect(owned, `${brand} ${app} ${key}`).toContain(value.slice(0, 7));
        }
        for (const rule of theme.rules) {
          expect(owned, `${brand} ${app} token ${rule.token}`).toContain(`#${rule.foreground ?? ''}`);
        }
      }
    }
  });

  it('leaves no Monaco default in the diff editor', () => {
    for (const app of APP_THEMES) {
      const colors = monacoTheme('crucible', app).colors;
      const diff = Object.keys(colors).filter((key) => key.startsWith('diffEditor'));
      expect(diff.length).toBeGreaterThan(6);
      for (const key of ['diffEditor.insertedTextBackground', 'diffEditor.removedTextBackground']) {
        expect(diff).toContain(key);
      }
    }
  });
});
