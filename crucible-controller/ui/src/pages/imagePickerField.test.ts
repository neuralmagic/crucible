import { describe, expect, it } from 'vitest';
import { selectedDigest } from './ImagePickerField';
import type { components } from '../api/schema';

type CatalogImageDto = components['schemas']['CatalogImageDto'];

const image = (name: string, digest: string, tags: string[]): CatalogImageDto => ({
  name,
  verified: true,
  repository: `ghcr.io/acme/${name}`,
  digest,
  tags,
  arches: ['amd64'],
  created_at: null,
  capabilities: null,
  capability_digest: null,
  intro_digest: null,
  first_seen: '',
  last_seen: '',
});

const IMAGES = [image('go-cc', 'sha256:aaa', ['latest', 'abc']), image('rust-cc', 'sha256:bbb', ['abc'])];

describe('selectedDigest', () => {
  it('matches a digest-pinned reference by digest', () => {
    expect(selectedDigest('ghcr.io/acme/go-cc@sha256:aaa', IMAGES)).toBe('sha256:aaa');
    expect(selectedDigest('ghcr.io/acme/go-cc@sha256:zzz', IMAGES)).toBe('');
  });
  it('matches a tag reference through the tags the digest carries', () => {
    expect(selectedDigest('ghcr.io/acme/go-cc:latest', IMAGES)).toBe('sha256:aaa');
    expect(selectedDigest('ghcr.io/acme/go-cc', IMAGES)).toBe('sha256:aaa');
    expect(selectedDigest('ghcr.io/acme/rust-cc:abc', IMAGES)).toBe('sha256:bbb');
    expect(selectedDigest('ghcr.io/acme/rust-cc:latest', IMAGES)).toBe('');
  });
  it('treats a missing or unknown reference as custom', () => {
    expect(selectedDigest(null, IMAGES)).toBe('');
    expect(selectedDigest('quay.io/other/x:1', IMAGES)).toBe('');
  });
});
