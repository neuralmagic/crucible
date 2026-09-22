// Theme bridge for the embedded flow report. The engine's flow.html declares its palette as
// --flow-* custom properties on :root; we splice a second :root block into the document we hand
// the iframe, mapped from the SPA's live theme tokens, so the report follows the app's
// light/dark toggle. Reports published before the palette was tokenized carry hardcoded hexes and
// simply ignore the override.

export type FlowTheme = 'light' | 'dark';

/** One --flow-* property: which theme token to read, and what to use when it resolves empty. */
interface FlowVar {
  readonly prop: string;
  readonly token: string | null;
  readonly light: string;
  readonly dark: string;
}

const FLOW_VARS: readonly FlowVar[] = [
  { prop: '--flow-bg', token: '--color-paper', light: '#edebe2', dark: '#0a0a09' },
  { prop: '--flow-surface', token: '--color-surface', light: '#ffffff', dark: '#1c1c19' },
  { prop: '--flow-ink', token: '--color-ink', light: '#100f0e', dark: '#f5f3ea' },
  { prop: '--flow-ink-2', token: '--color-ink-2', light: '#464439', dark: '#adaa9d' },
  { prop: '--flow-muted', token: '--color-ink-3', light: '#78756a', dark: '#7b786c' },
  { prop: '--flow-hairline', token: '--color-rule', light: '#c4c0b0', dark: '#3a3830' },
  { prop: '--flow-accent', token: '--color-blue', light: '#1a4785', dark: '#83afe8' },
  { prop: '--flow-line', token: '--color-rule', light: '#c4c0b0', dark: '#3a3830' },
  { prop: '--flow-line-soft', token: '--color-rule-hard', light: '#8a8676', dark: '#666254' },
  { prop: '--flow-badge-border', token: '--color-rule-hard', light: '#8a8676', dark: '#666254' },
  // The accent's deeper shade, the swimlane fills and on-accent text carry no theme token: none
  // holds the right contrast against the report's surface, so they are picked per theme.
  { prop: '--flow-accent-deep', token: null, light: '#0f2c53', dark: '#b8d3f2' },
  { prop: '--flow-pass-fill', token: null, light: '#dddacd', dark: '#3a3830' },
  { prop: '--flow-fail-fill', token: null, light: '#464439', dark: '#8a8676' },
  { prop: '--flow-dot', token: null, light: '#78756a', dark: '#7b786c' },
  { prop: '--flow-on-accent', token: null, light: '#edebe2', dark: '#0a0a09' },
];

/** A CSS custom-property reader, i.e. `getComputedStyle(el).getPropertyValue`. */
export type TokenReader = (token: string) => string;

/** Resolve every --flow-* value for `theme`, preferring the live theme token over the fallback. */
export function resolveFlowVars(theme: FlowTheme, read: TokenReader): Map<string, string> {
  const out = new Map<string, string>();
  for (const v of FLOW_VARS) {
    const live = v.token === null ? '' : read(v.token).trim();
    out.set(v.prop, live === '' ? (theme === 'dark' ? v.dark : v.light) : live);
  }
  return out;
}

/** The `<style>` element the report gets, including a matching color-scheme for its form
 * controls and scrollbars. `html:root` outranks the report's own `:root` defaults regardless of
 * stylesheet order. */
export function flowThemeStyle(theme: FlowTheme, read: TokenReader): string {
  const decls = [...resolveFlowVars(theme, read)]
    .map(([prop, value]) => `  ${prop}: ${value};`)
    .join('\n');
  return `<style id="flow-theme-bridge">\nhtml:root {\n  color-scheme: ${theme};\n${decls}\n}\n</style>`;
}

const HEAD_END = /<\/head\s*>/i;

/** Splice the bridge style in as the head's last child. A document without a closing head tag is
 * returned untouched: an unstyled-but-correct report beats a mangled one. */
export function injectFlowTheme(html: string, style: string): string {
  const m = HEAD_END.exec(html);
  if (m === null) return html;
  return `${html.slice(0, m.index)}${style}\n${html.slice(m.index)}`;
}
