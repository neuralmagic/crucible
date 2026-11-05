import Anser from 'anser';

/// The eight ANSI colours, each bright one folded onto its base. global.css maps every one onto a
/// theme token, so a log reads in the app's colours in both themes.
export type AnsiColor = 'black' | 'red' | 'green' | 'yellow' | 'blue' | 'magenta' | 'cyan' | 'white';

const COLORS: readonly AnsiColor[] = ['black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white'];

export interface AnsiPart {
  text: string;
  fg: AnsiColor | null;
  bg: AnsiColor | null;
  bold: boolean;
  dim: boolean;
  italic: boolean;
  underline: boolean;
}

function color(name: string | null): AnsiColor | null {
  if (name === null) return null;
  const base = name.replace(/^ansi-(bright-)?/, '');
  return COLORS.find((c) => c === base) ?? null;
}

/// `text` split at its SGR escapes into styled parts. Other escapes, and colours outside the
/// sixteen, are dropped.
export function ansiParts(text: string): AnsiPart[] {
  return Anser.ansiToJson(text, { json: true, remove_empty: true, use_classes: true }).map((entry) => ({
    text: entry.content,
    fg: color(entry.fg),
    bg: color(entry.bg),
    bold: entry.decorations.includes('bold'),
    dim: entry.decorations.includes('dim'),
    italic: entry.decorations.includes('italic'),
    underline: entry.decorations.includes('underline'),
  }));
}

/// What `text` reads as once rendered: its parts without their escapes.
export function ansiPlain(text: string): string {
  return ansiParts(text)
    .map((part) => part.text)
    .join('');
}

/// The global.css classes that style `part`, empty for plain text.
export function ansiClasses(part: AnsiPart): string {
  return [
    part.fg === null ? null : `ansi-${part.fg}`,
    part.bg === null ? null : `ansi-bg-${part.bg}`,
    part.bold ? 'ansi-bold' : null,
    part.dim ? 'ansi-dim' : null,
    part.italic ? 'ansi-italic' : null,
    part.underline ? 'ansi-underline' : null,
  ]
    .filter((c) => c !== null)
    .join(' ');
}
