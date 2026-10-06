import { describe, expect, it } from 'vitest';
import {
  buildLabel,
  byReason,
  imageChoices,
  newestFirst,
  unavailableImages,
  type BuildRow,
} from './imageChoices';

const SHA_A = 'fd2438cd09d2ac0bd3d467e8ad25d511f06f9a4e';
const SHA_B = '0a1b2c3d4e5f60718293a4b5c6d7e8f901234567';

function build(name: string, digest: string, over: Partial<BuildRow> = {}): BuildRow {
  return {
    repository: `ghcr.io/acme/${name}`,
    name,
    digest,
    tags: [],
    created_at: null,
    first_seen: '2026-09-01T00:00:00Z',
    ...over,
  };
}

describe('buildLabel', () => {
  it('prefers latest, then a named channel, then the short commit', () => {
    expect(buildLabel({ tags: [SHA_A, 'latest'] })).toBe('latest');
    expect(buildLabel({ tags: [SHA_A, 'v1.2.3'] })).toBe('v1.2.3');
    expect(buildLabel({ tags: [SHA_A] })).toBe('fd2438c');
    expect(buildLabel({ tags: [] })).toBe('untagged');
  });
});

describe('newestFirst', () => {
  it('puts the latest build first regardless of time', () => {
    const old = build('go', 'sha256:a', { tags: ['latest'], created_at: '2026-01-01T00:00:00Z' });
    const fresh = build('go', 'sha256:b', { tags: [SHA_A], created_at: '2026-09-01T00:00:00Z' });
    expect([fresh, old].sort(newestFirst).map((b) => b.digest)).toEqual(['sha256:a', 'sha256:b']);
  });

  it('orders by created time, then first seen, then digest', () => {
    const rows = [
      build('go', 'sha256:c', { created_at: null, first_seen: '2026-09-03T00:00:00Z' }),
      build('go', 'sha256:a', { created_at: '2026-09-01T00:00:00Z' }),
      build('go', 'sha256:b', { created_at: '2026-09-02T00:00:00Z' }),
      build('go', 'sha256:e', { created_at: null, first_seen: '2026-09-01T00:00:00Z' }),
      build('go', 'sha256:d', { created_at: null, first_seen: '2026-09-01T00:00:00Z' }),
    ];
    expect([...rows].sort(newestFirst).map((b) => b.digest)).toEqual([
      'sha256:b',
      'sha256:a',
      'sha256:c',
      'sha256:d',
      'sha256:e',
    ]);
  });
});

describe('imageChoices', () => {
  it('collapses every build of a repository into one choice, newest first, in rank order', () => {
    const choices = imageChoices(
      [
        { image: build('rust', 'sha256:r1', { tags: [SHA_B] }), default: false },
        { image: build('go', 'sha256:g1', { tags: [SHA_A], created_at: '2026-09-01T00:00:00Z' }), default: false },
        { image: build('go', 'sha256:g2', { tags: ['latest', SHA_B] }), default: true },
        { image: build('go', 'sha256:g3', { tags: [SHA_B], created_at: '2026-09-02T00:00:00Z' }), default: false },
      ],
      [],
      [],
    );
    expect(choices.map((c) => c.name)).toEqual(['rust', 'go']);
    expect(choices[1]?.builds.map((b) => b.image.digest)).toEqual(['sha256:g2', 'sha256:g3', 'sha256:g1']);
    expect(choices[1]?.newest.digest).toBe('sha256:g2');
    expect(choices.map((c) => c.isDefault)).toEqual([false, true]);
  });

  it('lists incompatible builds of a pickable image with why, and pins the newest compatible one', () => {
    const [go] = imageChoices(
      [{ image: build('go', 'sha256:old', { created_at: '2026-08-01T00:00:00Z' }), default: false }],
      [
        {
          image: build('go', 'sha256:new', { tags: ['latest'], created_at: '2026-09-01T00:00:00Z' }),
          unsatisfied: [{ predicate: 'toolchain.go', required: '>=1.25', found: '1.24' }],
        },
      ],
      [build('go', 'sha256:bare', { created_at: '2026-08-15T00:00:00Z' })],
    );
    expect(go?.builds.map((b) => [b.image.digest, b.reason])).toEqual([
      ['sha256:new', 'toolchain.go 1.24 ≠ >=1.25'],
      ['sha256:bare', 'unverified'],
      ['sha256:old', null],
    ]);
    expect(go?.newest.digest).toBe('sha256:old');
  });

  it('offers no choice for a repository with no compatible build', () => {
    expect(
      imageChoices(
        [],
        [{ image: build('rust', 'sha256:r'), unsatisfied: [{ predicate: 'toolchain.go', required: '>=1.25', found: null }] }],
        [build('custom', 'sha256:c')],
      ),
    ).toEqual([]);
  });
});

describe('unavailableImages', () => {
  it('lists each repository once, with the reason for its newest build', () => {
    const rows = unavailableImages(
      [
        {
          image: build('rust', 'sha256:r-old', { created_at: '2026-01-01T00:00:00Z' }),
          unsatisfied: [{ predicate: 'toolchain.go', required: '>=1.25', found: null }],
        },
        {
          image: build('rust', 'sha256:r-new', { created_at: '2026-09-01T00:00:00Z' }),
          unsatisfied: [{ predicate: 'toolchain.go', required: '>=1.25', found: '1.22' }],
        },
      ],
      [build('custom', 'sha256:c1'), build('custom', 'sha256:c2')],
      [],
    );
    expect(rows).toEqual([
      { repository: 'ghcr.io/acme/custom', name: 'custom', reason: 'unverified' },
      { repository: 'ghcr.io/acme/rust', name: 'rust', reason: 'toolchain.go 1.22 ≠ >=1.25' },
    ]);
  });

  it('joins several unmet predicates', () => {
    const [row] = unavailableImages(
      [
        {
          image: build('rust', 'sha256:r'),
          unsatisfied: [
            { predicate: 'toolchain.go', required: '>=1.25', found: null },
            { predicate: 'agent.codex', required: '*', found: null },
          ],
        },
      ],
      [],
      [],
    );
    expect(row?.reason).toBe('lacks toolchain.go, lacks agent.codex');
  });

  it('leaves out a repository that has a compatible build', () => {
    const rows = unavailableImages(
      [{ image: build('go', 'sha256:old'), unsatisfied: [{ predicate: 'toolchain.go', required: '>=1.25', found: '1.20' }] }],
      [build('go', 'sha256:bare')],
      [{ repository: 'ghcr.io/acme/go' }],
    );
    expect(rows).toEqual([]);
  });
});

describe('byReason', () => {
  const row = (name: string, reason: string) => ({ repository: `ghcr.io/acme/${name}`, name, reason });

  it('groups by reason, largest first, keeping the given order inside a group', () => {
    const groups = byReason([
      row('custom', 'unverified'),
      row('go-codex', 'lacks agent.claude-code'),
      row('rust-cc', 'lacks toolchain.go'),
      row('rust-pi', 'lacks agent.claude-code'),
      row('vllm-codex', 'lacks agent.claude-code'),
    ]);
    expect(groups.map((g) => [g.reason, g.rows.map((r) => r.name)])).toEqual([
      ['lacks agent.claude-code', ['go-codex', 'rust-pi', 'vllm-codex']],
      ['lacks toolchain.go', ['rust-cc']],
      ['unverified', ['custom']],
    ]);
  });

  it('is empty for no rows', () => {
    expect(byReason([])).toEqual([]);
  });
});
