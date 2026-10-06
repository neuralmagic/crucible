import { describe, expect, it } from 'vitest';
import { shownLinks, type ExternalLinkRef } from './ExternalLink';

const link = (url: string, label: string): ExternalLinkRef => ({
  url,
  provider: 'github',
  kind: 'pull_request',
  label,
});

describe('shownLinks', () => {
  it('keeps the first of each url and the order they came in', () => {
    const pr = link('https://github.com/o/r/pull/1', '#1');
    const branch = link('https://github.com/o/r/tree/topic', 'topic');
    expect(shownLinks([pr, branch, { ...pr, label: 'again' }])).toEqual([pr, branch]);
  });

  it('drops anything that is not an http(s) url', () => {
    const urls = ['javascript:alert(1)', 'data:text/html,x', 'file:///etc/passwd', '/runs/1', ''];
    expect(shownLinks(urls.map((url) => link(url, 'x')))).toEqual([]);
    expect(shownLinks([link('HTTP://example.com/x', 'x')])).toHaveLength(1);
  });

  it('is empty for no links', () => {
    expect(shownLinks([])).toEqual([]);
  });
});
