import { describe, expect, it } from 'vitest';
import {
  activityLabel,
  appendLog,
  parseActivity,
  parseProgress,
  phaseColor,
  upsertBeat,
  type LogLine,
  type ScopeActivityBeat,
  type ScopeProgressBeat,
} from './turnStream';

describe('parseProgress', () => {
  it('decodes a well-formed beat', () => {
    const beat = parseProgress(
      '{"round":2,"kind":"refine","doing":"refining the pack","cost_so_far":0.42}',
    );
    expect(beat).toEqual({ round: 2, kind: 'refine', doing: 'refining the pack', cost_so_far: 0.42 });
  });

  it('accepts every round kind the engine emits', () => {
    for (const kind of ['propose', 'refine', 'adversary']) {
      const beat = parseProgress(`{"round":1,"kind":"${kind}","doing":"x","cost_so_far":0}`);
      expect(beat?.kind).toBe(kind);
    }
  });

  it('rejects non-JSON, wrong shapes, and unknown kinds', () => {
    expect(parseProgress('not json')).toBeNull();
    expect(parseProgress('42')).toBeNull();
    expect(parseProgress('null')).toBeNull();
    expect(parseProgress('{"round":"two","kind":"refine","doing":"x","cost_so_far":0}')).toBeNull();
    expect(parseProgress('{"round":0,"kind":"refine","doing":"x","cost_so_far":0}')).toBeNull();
    expect(parseProgress('{"round":1,"kind":"exfiltrate","doing":"x","cost_so_far":0}')).toBeNull();
    expect(parseProgress('{"round":1,"kind":"refine","cost_so_far":0}')).toBeNull();
    expect(parseProgress('{"round":1,"kind":"refine","doing":"x","cost_so_far":"free"}')).toBeNull();
  });
});

describe('parseActivity', () => {
  it('decodes a tool beat with its name', () => {
    const beat = parseActivity(
      '{"kind":"tool","name":"Edit","detail":"router.go: rebalance","cost_so_far":0.12}',
    );
    expect(beat).toEqual({ kind: 'tool', name: 'Edit', detail: 'router.go: rebalance', cost_so_far: 0.12 });
  });

  it('accepts every kind the engine emits, name optional', () => {
    for (const kind of ['tool', 'text', 'usage', 'stage', 'truncated']) {
      const beat = parseActivity(`{"kind":"${kind}","detail":"x","cost_so_far":0}`);
      expect(beat?.kind).toBe(kind);
      expect(beat?.name).toBeUndefined();
    }
  });

  it('rejects non-JSON, wrong shapes, and unknown kinds', () => {
    expect(parseActivity('not json')).toBeNull();
    expect(parseActivity('[]')).toBeNull();
    expect(parseActivity('{"kind":"exfiltrate","detail":"x","cost_so_far":0}')).toBeNull();
    expect(parseActivity('{"kind":"tool","cost_so_far":0}')).toBeNull();
    expect(parseActivity('{"kind":"tool","name":7,"detail":"x","cost_so_far":0}')).toBeNull();
    expect(parseActivity('{"kind":"tool","detail":"x","cost_so_far":"free"}')).toBeNull();
  });
});

describe('activityLabel', () => {
  const beat = (kind: ScopeActivityBeat['kind'], detail: string, name?: string): ScopeActivityBeat =>
    name === undefined ? { kind, detail, cost_so_far: 0 } : { kind, name, detail, cost_so_far: 0 };

  it('prefixes tool beats with the tool name', () => {
    expect(activityLabel(beat('tool', 'crucible.toml', 'Read'))).toBe('Read · crucible.toml');
    expect(activityLabel(beat('tool', 'crucible.toml'))).toBe('crucible.toml');
  });

  it('quotes text snippets and passes the rest through', () => {
    expect(activityLabel(beat('text', 'drafting the gate'))).toBe('“drafting the gate”');
    expect(activityLabel(beat('usage', '18400 tokens'))).toBe('18400 tokens');
    expect(activityLabel(beat('stage', 'creating sandbox'))).toBe('creating sandbox');
    expect(activityLabel(beat('truncated', 'activity feed truncated'))).toBe('activity feed truncated');
  });
});

describe('upsertBeat', () => {
  const beat = (round: number, kind: ScopeProgressBeat['kind'], cost = 0): ScopeProgressBeat => ({
    round,
    kind,
    doing: `${kind} r${round}`,
    cost_so_far: cost,
  });

  it('appends new beats sorted by round', () => {
    let rail: ScopeProgressBeat[] = [];
    rail = upsertBeat(rail, beat(2, 'refine'));
    rail = upsertBeat(rail, beat(1, 'propose'));
    expect(rail.map((b) => b.round)).toEqual([1, 2]);
  });

  it('replaces a replayed (round, kind) beat instead of duplicating', () => {
    let rail = [beat(1, 'propose', 0)];
    rail = upsertBeat(rail, beat(1, 'propose', 0.1));
    expect(rail).toHaveLength(1);
    expect(rail[0].cost_so_far).toBe(0.1);
  });

  it('keeps distinct kinds within one round', () => {
    let rail = [beat(2, 'adversary')];
    rail = upsertBeat(rail, beat(2, 'refine'));
    expect(rail).toHaveLength(2);
  });

  it('never mutates its input', () => {
    const rail = [beat(1, 'propose')];
    upsertBeat(rail, beat(2, 'refine'));
    expect(rail).toHaveLength(1);
  });
});

describe('appendLog', () => {
  it('appends and caps, dropping the oldest', () => {
    let lines: LogLine[] = [];
    for (let i = 1; i <= 5; i++) {
      lines = appendLog(lines, i, `line ${i}`, 3);
    }
    expect(lines.map((l) => l.id)).toEqual([3, 4, 5]);
  });

  it('never mutates its input', () => {
    const lines = [{ id: 1, text: 'a' }];
    appendLog(lines, 2, 'b', 10);
    expect(lines).toHaveLength(1);
  });
});

describe('phaseColor', () => {
  it('maps known phases and defaults grey', () => {
    expect(phaseColor('Running')).toBe('blue');
    expect(phaseColor('Succeeded')).toBe('green');
    expect(phaseColor('Failed')).toBe('red');
    expect(phaseColor('Pending')).toBe('grey');
    expect(phaseColor('SomethingNew')).toBe('grey');
  });
});
