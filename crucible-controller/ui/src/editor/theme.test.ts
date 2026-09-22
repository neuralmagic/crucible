import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import { monacoTheme, PALETTE_KEYS, PALETTES, TOKEN_OF } from './theme';

const CSS = readFileSync(fileURLToPath(new URL('../global.css', import.meta.url)), 'utf8');

/// Every `--color-*` declared in one CSS block, keyed by custom property.
function block(open: string): Map<string, string> {
  const start = CSS.indexOf(open);
  expect(start, `${open} is declared in global.css`).toBeGreaterThan(-1);
  const body = CSS.slice(start + open.length, CSS.indexOf('}', start));
  const declared = new Map<string, string>();
  for (const line of body.split(';')) {
    const [name, value] = line.split(':');
    if (name === undefined || value === undefined) continue;
    const property = name.trim();
    if (property.startsWith('--color-')) declared.set(property, value.trim());
  }
  return declared;
}

const BLOCKS: Record<'light' | 'dark', Map<string, string>> = {
  light: block('@theme {'),
  dark: block(":root[data-theme='dark'] {"),
};

describe('the Monaco palettes', () => {
  for (const app of ['light', 'dark'] as const) {
    it(`is the app's own ${app} tokens, to the byte`, () => {
      const declared = BLOCKS[app];
      const palette = PALETTES[app];
      for (const key of PALETTE_KEYS) {
        const token = TOKEN_OF[key];
        expect(palette[key], `${key} follows ${token}`).toBe(declared.get(token));
      }
    });
  }

  it('paints no colour the app does not own', () => {
    for (const app of ['light', 'dark'] as const) {
      const owned = new Set([...PALETTE_KEYS.map((key) => PALETTES[app][key]), '#000000']);
      const theme = monacoTheme(app);
      for (const [key, value] of Object.entries(theme.colors)) {
        // Anything faded keeps the token it was faded from.
        expect(owned, `${app} ${key}`).toContain(value.slice(0, 7));
      }
      for (const rule of theme.rules) {
        expect(owned, `${app} token ${rule.token}`).toContain(`#${rule.foreground ?? ''}`);
      }
    }
  });

  it('leaves no Monaco default in the diff editor', () => {
    for (const app of ['light', 'dark'] as const) {
      const colors = monacoTheme(app).colors;
      const diff = Object.keys(colors).filter((key) => key.startsWith('diffEditor'));
      expect(diff.length).toBeGreaterThan(6);
      for (const key of ['diffEditor.insertedTextBackground', 'diffEditor.removedTextBackground']) {
        expect(diff).toContain(key);
      }
    }
  });
});
