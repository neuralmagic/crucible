import { describe, expect, it } from 'vitest';
import { parseSessionLine, parseStatus } from './session';

describe('parseSessionLine', () => {
  it('parses the major kinds with defaults for absent fields', () => {
    const start = parseSessionLine('{"kind":"start","goal":"g","gate":"bench","max_cost":10}');
    expect(start).toMatchObject({ kind: 'start', goal: 'g', gate: 'bench', max_cost: 10, iters_total: 0 });

    const row = parseSessionLine('{"kind":"row","row":{"iter":2,"decision":"keep","score":1.5,"phase":"deep"}}');
    expect(row).toMatchObject({ kind: 'row', row: { iter: 2, decision: 'keep', score: 1.5, phase: 'deep' } });

    const budget = parseSessionLine('{"kind":"budget","spent":1.2,"elapsed_secs":30}');
    expect(budget).toMatchObject({ kind: 'budget', spent: 1.2, elapsed_secs: 30 });
  });

  it('nests agent events and falls back per-layer on unknowns', () => {
    const tool = parseSessionLine('{"kind":"agent","event":{"kind":"tool","name":"Bash","summary":"ls"}}');
    expect(tool).toMatchObject({ kind: 'agent', event: { kind: 'tool', name: 'Bash' } });

    const unknownAgent = parseSessionLine('{"kind":"agent","event":{"kind":"novel-thing","x":1}}');
    if (unknownAgent.kind !== 'agent') throw new Error('expected agent');
    expect(unknownAgent.event.kind).toBe('unknown');
  });

  it('never throws: garbage becomes an unknown line carrying the raw text', () => {
    for (const garbage of ['not json', '', '42', '[1,2]', '{"no_kind":true}']) {
      const parsed = parseSessionLine(garbage);
      expect(parsed.kind).toBe('unknown');
    }
  });
});

describe('parseStatus', () => {
  it('parses a full snapshot', () => {
    const s = parseStatus('{"phase":"deep","iter":3,"best_score":290,"spend":1.25,"paused":false,"max_cost":10}');
    expect(s).toMatchObject({ phase: 'deep', iter: 3, best_score: 290, spend: 1.25, paused: false, max_cost: 10 });
  });

  it('returns null for garbage rather than a half-filled snapshot', () => {
    expect(parseStatus('nope')).toBeNull();
    expect(parseStatus('[]')).toBeNull();
  });
});
