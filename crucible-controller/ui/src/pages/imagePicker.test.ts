import { describe, expect, it } from 'vitest';
import { pinnedReference, readSandboxImage, setSandboxImage, shortDigest } from './imagePicker';

const WITH_IMAGE = [
  '[repo]',
  'path = "."',
  '',
  '[agent]',
  'backend = "openshell"',
  'sandbox_image = "ghcr.io/acme/old:tag"   # pinned by hand',
  '',
  '[agent.requires]',
  '"toolchain.go" = ">=1.25"',
  '',
  '[workflow]',
  'type = "playbook"',
  '',
].join('\n');

describe('readSandboxImage', () => {
  it('reads the image under [agent] and ignores other tables', () => {
    expect(readSandboxImage(WITH_IMAGE)).toBe('ghcr.io/acme/old:tag');
    expect(readSandboxImage('[workflow]\nsandbox_image = "x"\n')).toBeNull();
    expect(readSandboxImage('[agent]\nbackend = "local"\n')).toBeNull();
    expect(readSandboxImage('[agent]\nsandbox_image = ""\n')).toBeNull();
  });
});

describe('setSandboxImage', () => {
  it('rewrites the existing line in place and keeps its trailing comment', () => {
    const next = setSandboxImage(WITH_IMAGE, 'ghcr.io/acme/new@sha256:abc');
    expect(next).toContain('sandbox_image = "ghcr.io/acme/new@sha256:abc"   # pinned by hand');
    expect(next).not.toContain('old:tag');
    expect(next.split('\n').length).toBe(WITH_IMAGE.split('\n').length);
  });
  it('adds the line at the top of an [agent] table that has none', () => {
    const next = setSandboxImage('[agent]\nbackend = "openshell"\n\n[workflow]\n', 'r@sha256:1');
    expect(next).toBe('[agent]\nsandbox_image = "r@sha256:1"\nbackend = "openshell"\n\n[workflow]\n');
  });
  it('appends an [agent] table when the manifest has none', () => {
    expect(setSandboxImage('[repo]\npath = "."\n', 'r@sha256:1')).toBe(
      '[repo]\npath = "."\n\n[agent]\nsandbox_image = "r@sha256:1"\n',
    );
    expect(setSandboxImage('', 'r@sha256:1')).toBe('[agent]\nsandbox_image = "r@sha256:1"\n');
  });
  it('does not confuse [agent.requires] with the agent table', () => {
    const next = setSandboxImage('[agent.requires]\n"a" = "1"\n', 'r@sha256:1');
    expect(next).toBe('[agent.requires]\n"a" = "1"\n\n[agent]\nsandbox_image = "r@sha256:1"\n');
  });
});

describe('references', () => {
  it('pins by digest and shortens for display', () => {
    expect(pinnedReference('ghcr.io/acme/x', 'sha256:abcdef0123456789ff')).toBe(
      'ghcr.io/acme/x@sha256:abcdef0123456789ff',
    );
    expect(shortDigest('sha256:abcdef0123456789ff')).toBe('abcdef012345');
  });
});
