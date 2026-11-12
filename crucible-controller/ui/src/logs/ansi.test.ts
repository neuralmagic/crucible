import { describe, expect, it } from 'vitest';
import { ansiClasses, ansiParts, ansiPlain } from './ansi';

const TRACING =
  '\x1b[2m2026-11-04T20:04:14.524641Z\x1b[0m \x1b[32m INFO\x1b[0m \x1b[1mserve_inner\x1b[0m\x1b[2m:\x1b[0m task cancelled';

describe('ansiParts', () => {
  it('splits a tracing line at its escapes and keeps every character it shows', () => {
    const parts = ansiParts(TRACING);
    expect(parts.map((p) => p.text).join('')).toBe(
      '2026-11-04T20:04:14.524641Z  INFO serve_inner: task cancelled'
    );
    expect(parts.find((p) => p.text === '2026-11-04T20:04:14.524641Z')).toMatchObject({ dim: true, fg: null });
    expect(parts.find((p) => p.text === ' INFO')).toMatchObject({ fg: 'green', bold: false });
    expect(parts.find((p) => p.text === 'serve_inner')).toMatchObject({ bold: true, fg: null });
    expect(parts.find((p) => p.text === ' task cancelled')).toMatchObject({
      fg: null,
      bg: null,
      bold: false,
      dim: false,
      italic: false,
      underline: false,
    });
  });

  it('folds bright colours onto their base and reads backgrounds and decorations', () => {
    const [part] = ansiParts('\x1b[1;3;4;91;42mloud\x1b[0m');
    expect(part).toEqual({
      text: 'loud',
      fg: 'red',
      bg: 'green',
      bold: true,
      dim: false,
      italic: true,
      underline: true,
    });
  });

  it('drops colours outside the sixteen and escapes that are not SGR', () => {
    const parts = ansiParts('\x1b[38;5;208morange\x1b[0m\x1b[2Kcleared');
    expect(parts.map((p) => p.text).join('')).toBe('orangecleared');
    expect(parts.every((p) => p.fg === null)).toBe(true);
  });

  it('leaves text without escapes whole and unstyled', () => {
    expect(ansiParts('plain <b>&amp;</b>')).toEqual([
      { text: 'plain <b>&amp;</b>', fg: null, bg: null, bold: false, dim: false, italic: false, underline: false },
    ]);
  });
});

describe('ansiPlain', () => {
  it('reads as the rendered text', () => {
    expect(ansiPlain(TRACING)).toBe('2026-11-04T20:04:14.524641Z  INFO serve_inner: task cancelled');
    expect(ansiPlain('')).toBe('');
  });

  it('keeps every line of a log and its line breaks', () => {
    expect(ansiPlain('\x1b[31merror\x1b[0m: no such file\n\n[2/3] \x1b[1mBuilding\x1b[0m\n')).toBe(
      'error: no such file\n\n[2/3] Building\n',
    );
  });
});

describe('ansiClasses', () => {
  it('names a class per style and none for plain text', () => {
    const [loud] = ansiParts('\x1b[1;2;3;4;35;46mx\x1b[0m');
    expect(ansiClasses(loud)).toBe('ansi-magenta ansi-bg-cyan ansi-bold ansi-dim ansi-italic ansi-underline');
    const [plain] = ansiParts('x');
    expect(ansiClasses(plain)).toBe('');
  });
});
