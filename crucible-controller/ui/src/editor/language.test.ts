import { describe, expect, it } from 'vitest';
import { languageOf } from './language';

describe('languageOf', () => {
  it('routes a pack by filename', () => {
    expect(languageOf('workflow.star')).toBe('python');
    expect(languageOf('crucible.toml')).toBe('toml');
    expect(languageOf('skills/read/SKILL.md')).toBe('markdown');
    expect(languageOf('params.json')).toBe('json');
    expect(languageOf('settle.sh')).toBe('shell');
    expect(languageOf('deploy.yaml')).toBe('yaml');
  });

  it('falls back to plain text rather than guessing', () => {
    expect(languageOf('LICENSE')).toBe('plaintext');
    expect(languageOf('notes')).toBe('plaintext');
    expect(languageOf('archive.tar.gz')).toBe('plaintext');
  });

  it('reads the extension off the basename, not the directory', () => {
    expect(languageOf('skills.md/read')).toBe('plaintext');
    expect(languageOf('a.b.c/workflow.star')).toBe('python');
  });

  it('ignores extension case and a leading dot', () => {
    expect(languageOf('README.MD')).toBe('markdown');
    expect(languageOf('.gitignore')).toBe('plaintext');
  });
});
