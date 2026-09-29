import { describe, expect, it } from 'vitest';
import { sourceLabel } from './playbookSource';

describe('sourceLabel', () => {
  it('names a git pack by repo and directory, and a published draft by its version', () => {
    expect(sourceLabel({ kind: 'git', repo: 'owner/packs', git_ref: null, path: '' })).toBe(
      'owner/packs'
    );
    expect(
      sourceLabel({ kind: 'git', repo: 'owner/packs', git_ref: 'main', path: 'packs/survey' })
    ).toBe('owner/packs/packs/survey');
    expect(sourceLabel({ kind: 'draft', draft: 'studio', version: 3 })).toBe('draft studio v3');
  });
});
