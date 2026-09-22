import { describe, expect, it } from 'vitest';
import { flowThemeStyle, injectFlowTheme, resolveFlowVars } from './flowTheme';

const noTokens: (token: string) => string = () => '';

describe('resolveFlowVars', () => {
  it('falls back per theme when a token resolves empty', () => {
    const light = resolveFlowVars('light', noTokens);
    const dark = resolveFlowVars('dark', noTokens);
    expect(light.get('--flow-bg')).toBe('#edebe2');
    expect(dark.get('--flow-bg')).toBe('#0a0a09');
    expect(light.get('--flow-ink')).toBe('#100f0e');
    expect(dark.get('--flow-ink')).toBe('#f5f3ea');
  });

  it('prefers the live token and trims the computed value', () => {
    const vars = resolveFlowVars('dark', (token) =>
      token === '--color-blue' ? ' #ff00ff ' : '',
    );
    expect(vars.get('--flow-accent')).toBe('#ff00ff');
    expect(vars.get('--flow-accent-deep')).toBe('#b8d3f2');
  });

  it('covers the emitter palette', () => {
    const vars = resolveFlowVars('light', noTokens);
    for (const prop of [
      '--flow-bg',
      '--flow-surface',
      '--flow-ink',
      '--flow-ink-2',
      '--flow-muted',
      '--flow-hairline',
      '--flow-accent',
      '--flow-accent-deep',
      '--flow-pass-fill',
      '--flow-fail-fill',
      '--flow-dot',
      '--flow-line',
      '--flow-line-soft',
      '--flow-badge-border',
      '--flow-on-accent',
    ]) {
      expect(vars.get(prop)).toMatch(/^#[0-9a-f]{6}$/);
    }
  });
});

describe('flowThemeStyle', () => {
  it('emits one html:root block carrying the color-scheme', () => {
    const style = flowThemeStyle('dark', noTokens);
    expect(style).toContain('<style id="flow-theme-bridge">');
    expect(style).toContain('color-scheme: dark;');
    expect(style).toContain('--flow-bg: #0a0a09;');
    expect(style.match(/html:root/g)).toHaveLength(1);
    expect(flowThemeStyle('light', noTokens)).toContain('color-scheme: light;');
  });
});

describe('injectFlowTheme', () => {
  const style = '<style id="flow-theme-bridge">html:root { --flow-bg: #000; }</style>';

  it('inserts the style as the head’s last child', () => {
    const out = injectFlowTheme('<html><head><style>a{}</style></head><body>x</body></html>', style);
    expect(out.indexOf(style)).toBeGreaterThan(out.indexOf('<style>a{}</style>'));
    expect(out.indexOf(style)).toBeLessThan(out.indexOf('</head>'));
    expect(out).toContain('<body>x</body>');
  });

  it('tolerates head tag attributes and a spaced closing tag', () => {
    const out = injectFlowTheme('<head lang="en"><title>t</title></HEAD >body', style);
    expect(out).toContain(`${style}\n</HEAD >`);
  });

  it('leaves a document without a closing head tag untouched', () => {
    const html = '<html><body>no head</body></html>';
    expect(injectFlowTheme(html, style)).toBe(html);
  });

  it('injects once even if the body mentions the tag text', () => {
    const out = injectFlowTheme('<head></head><body>&lt;/head&gt;</body>', style);
    expect(out.match(/flow-theme-bridge/g)).toHaveLength(1);
  });
});
