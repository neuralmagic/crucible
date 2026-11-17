import { describe, expect, it } from 'vitest';
import {
  ceilingHint,
  foldedChoices,
  initialValues,
  launchBody,
  parseParamsSchema,
  perValueKey,
  picked,
  runCount,
  validateParams,
  withPick,
  type ParamFieldSpec,
} from './playbookLaunchForm';

const SCHEMA = {
  type: 'object',
  required: ['release'],
  properties: {
    release: { type: 'string', enum: ['r1', 'r2', 'r3'], 'x-fan-out': true },
    component: {
      type: 'string',
      'x-choices-from': { task: 'opts', field: 'components', keyed_by: 'release' },
      'x-choices-snapshot': { value: { r1: ['gpu', 'tpu'], r2: ['gpu'] }, captured_at: null },
    },
    override: { type: 'boolean' },
    branch: {
      type: 'string',
      'x-enabled-by': 'override',
      'x-placeholder-from': { task: 'opts', field: 'branches', keyed_by: 'release' },
    },
    note: { type: 'string' },
  },
};

function specs(): ParamFieldSpec[] {
  const parsed = parseParamsSchema(SCHEMA);
  if (parsed.kind !== 'form') throw new Error(parsed.reason);
  return parsed.specs;
}

const CEILINGS = { maxCost: 2, maxTime: '30m', schemaDigest: 'sha256:s' };

describe('fan-out params', () => {
  it('reads x-fan-out, and refuses one that is not a string with offered values', () => {
    expect(specs().find((spec) => spec.name === 'release')?.fanOut).toBe(true);
    expect(parseParamsSchema({ properties: { r: { type: 'string', 'x-fan-out': true } } }).kind).toBe('unrenderable');
    expect(parseParamsSchema({ properties: { n: { type: 'integer', enum: [1], 'x-fan-out': true } } }).kind).toBe(
      'unrenderable'
    );
  });

  it('keeps the picks in the order picked and reads one plain value as one pick', () => {
    const [release] = specs();
    if (release === undefined) throw new Error('release');
    let values: Record<string, string> = { release: '' };
    values = { release: withPick(release, values, 'r2', true) };
    values = { release: withPick(release, values, 'r1', true) };
    expect(picked(release, values)).toEqual(['r2', 'r1']);
    expect(picked(release, { release: withPick(release, values, 'r2', false) })).toEqual(['r1']);
    expect(picked(release, { release: 'r3' })).toEqual(['r3']);
  });

  it('shows the first values of a long list and keeps a picked value past them in view', () => {
    const offered = ['a', 'b', 'c', 'd', 'e', 'f', 'g'];
    expect(foldedChoices(offered, [], 5)).toEqual({ shown: ['a', 'b', 'c', 'd', 'e'], hidden: 2 });
    expect(foldedChoices(offered, ['g'], 5)).toEqual({ shown: ['a', 'b', 'c', 'd', 'e', 'g'], hidden: 1 });
    expect(foldedChoices(offered, [], offered.length).hidden).toBe(0);
  });

  it('sends one picked value as it is, and several as an array with keyed params per value', () => {
    const all = specs();
    const one = { release: '["r1"]', [perValueKey('component', 'r1')]: 'tpu', note: 'n' };
    expect(launchBody(all, one, CEILINGS).params).toEqual({ release: 'r1', component: 'tpu', note: 'n' });

    const two = {
      release: '["r1","r2"]',
      [perValueKey('component', 'r1')]: 'tpu',
      [perValueKey('override', 'r2')]: 'true',
      [perValueKey('branch', 'r2')]: 'main',
      [perValueKey('branch', 'r1')]: 'ignored while r1 does not override',
      note: 'n',
    };
    expect(launchBody(all, two, CEILINGS).params).toEqual({
      release: '["r1","r2"]',
      component: '{"r1":"tpu"}',
      override: '{"r2":"true"}',
      branch: '{"r2":"main"}',
      note: 'n',
    });
    expect(runCount(all, two)).toBe(2);
  });

  it('validates a keyed param against each picked value and files the refusal under that value', () => {
    const all = specs();
    const errors = validateParams(all, {
      release: '["r1","r2"]',
      [perValueKey('component', 'r1')]: 'tpu',
      [perValueKey('component', 'r2')]: 'tpu',
    });
    expect(errors.get(perValueKey('component', 'r2'))).toBe('component must be one of gpu');
    expect(errors.has(perValueKey('component', 'r1'))).toBe(false);
    expect(validateParams(all, { release: '[]' }).get('release')).toBe('pick at least one release');
    expect(validateParams(all, { release: '["r9"]' }).get('release')).toBe('release must be one of r1, r2, r3');
  });

  it('prefills a relaunch of one run into that value’s own settings', () => {
    const values = initialValues(specs(), { release: 'r1', component: 'tpu' });
    expect(values[perValueKey('component', 'r1')]).toBe('tpu');
    expect(launchBody(specs(), values, CEILINGS).params).toEqual({ release: 'r1', component: 'tpu' });
  });

  it('says a ceiling applies to each run when the values launch several', () => {
    expect(ceilingHint('Capped at $5 per run.', 3)).toBe('Capped at $5 per run. Applies to each of the 3 runs.');
    expect(ceilingHint('Capped at $5 per run.', 1)).toBe('Capped at $5 per run.');
    expect(ceilingHint(null, 1)).toBeUndefined();
  });
});
