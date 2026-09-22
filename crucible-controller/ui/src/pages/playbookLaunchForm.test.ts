import { describe, expect, it } from 'vitest';
import {
  clampCeiling,
  initialValues,
  launchBody,
  mapServerRejection,
  parseParamsSchema,
  parseMaxTime,
  previewMatches,
  runSnapshot,
  scheduleBody,
  validateCronExpr,
  validateMaxTime,
  validateParam,
  type ParamFieldSpec,
} from './playbookLaunchForm';

/// The shape `crucible plan params` prints for a pack: string properties, a pattern, a default and
/// doc strings, with nothing undeclared allowed through.
const FIXTURE = {
  type: 'object',
  properties: {
    topic: { type: 'string', pattern: '^[a-z ]+$', description: 'What the survey covers.' },
    depth: { type: 'string', default: 'shallow' },
    since: { type: 'string', pattern: '^\\d{4}-\\d{2}-\\d{2}$', default: '2026-01-01' },
    note: { type: 'string' },
  },
  required: ['topic'],
  additionalProperties: false,
};

function specsOf(schema: unknown): ParamFieldSpec[] {
  const parsed = parseParamsSchema(schema);
  if (parsed.kind !== 'form') throw new Error(`unrenderable: ${parsed.reason}`);
  return parsed.specs;
}

describe('parseParamsSchema', () => {
  it('narrows the closed subset into one spec per property', () => {
    expect(specsOf(FIXTURE)).toEqual([
      {
        name: 'topic',
        required: true,
        defaultValue: null,
        pattern: '^[a-z ]+$',
        description: 'What the survey covers.',
      },
      { name: 'depth', required: false, defaultValue: 'shallow', pattern: null, description: null },
      {
        name: 'since',
        required: false,
        defaultValue: '2026-01-01',
        pattern: '^\\d{4}-\\d{2}-\\d{2}$',
        description: null,
      },
      { name: 'note', required: false, defaultValue: null, pattern: null, description: null },
    ]);
  });

  it('ignores keywords outside the subset and an absent required list', () => {
    const specs = specsOf({
      type: 'object',
      $schema: 'https://json-schema.org/draft/2020-12/schema',
      properties: { topic: { type: 'string', minLength: 3, examples: ['attention'] } },
      additionalProperties: false,
    });
    expect(specs).toEqual([
      { name: 'topic', required: false, defaultValue: null, pattern: null, description: null },
    ]);
  });

  it('renders an empty form for a pack that declares no params', () => {
    expect(specsOf({ type: 'object' })).toEqual([]);
  });

  it('refuses to render anything outside the subset', () => {
    for (const schema of [
      42,
      null,
      ['topic'],
      { type: 'array' },
      { type: 'object', properties: { n: { type: 'integer' } } },
      { type: 'object', properties: { n: { type: 'string', default: 3 } } },
      { type: 'object', properties: { n: { type: 'string', pattern: 7 } } },
      { type: 'object', properties: { n: 'string' } },
      { type: 'object', properties: { n: { type: 'string' } }, required: 'n' },
    ]) {
      expect(parseParamsSchema(schema).kind, JSON.stringify(schema)).toBe('unrenderable');
    }
  });
});

describe('initialValues', () => {
  it('prefills defaults and leaves the rest empty', () => {
    expect(initialValues(specsOf(FIXTURE))).toEqual({
      topic: '',
      depth: 'shallow',
      since: '2026-01-01',
      note: '',
    });
  });

  it('lets a relaunch snapshot win over a default', () => {
    expect(initialValues(specsOf(FIXTURE), { topic: 'attention', depth: 'deep', gone: 'x' })).toEqual({
      topic: 'attention',
      depth: 'deep',
      since: '2026-01-01',
      note: '',
    });
  });
});

describe('validateParam', () => {
  const specs = specsOf(FIXTURE);
  const topic = specs[0];
  const note = specs[3];
  const since = specs[2];

  it('requires a value only where the pack does', () => {
    expect(validateParam(topic, '   ')).toBe('topic is required');
    expect(validateParam(note, '')).toBeNull();
  });

  it('checks the pattern and names it', () => {
    expect(validateParam(topic, 'ATTENTION')).toBe('topic must match ^[a-z ]+$');
    expect(validateParam(topic, 'attention kernels')).toBeNull();
    expect(validateParam(since, '2026-13')).toBe('since must match ^\\d{4}-\\d{2}-\\d{2}$');
  });

  it('skips a pattern the browser cannot compile, leaving the endpoint to enforce it', () => {
    const rustOnly: ParamFieldSpec = {
      name: 'topic',
      required: true,
      defaultValue: null,
      pattern: '(?P<year>\\d{4})',
      description: null,
    };
    expect(validateParam(rustOnly, 'whatever')).toBeNull();
  });
});

describe('launchBody', () => {
  it('trims values, omits blank optionals and carries the ceilings', () => {
    const body = launchBody(
      specsOf(FIXTURE),
      { topic: '  attention  ', depth: 'deep', since: '', note: '' },
      { maxCost: 4.5, maxTime: ' 30m ', schemaDigest: 'sha256:beef' }
    );
    expect(body).toEqual({
      params: { topic: 'attention', depth: 'deep' },
      max_cost: 4.5,
      max_time: '30m',
      schema_digest: 'sha256:beef',
    });
  });

  it('names no provider: a playbook run takes its agent from the pack manifest', () => {
    const body = launchBody(specsOf(FIXTURE), {}, {
      maxCost: 1,
      maxTime: '90s',
      schemaDigest: 'sha256:beef',
    });
    expect('provider' in body).toBe(false);
    expect('model' in body).toBe(false);
  });

  it('sends a blank required param so the endpoint answers for it', () => {
    const body = launchBody(specsOf(FIXTURE), {}, {
      maxCost: 1,
      maxTime: '90s',
      schemaDigest: 'sha256:beef',
    });
    expect(body.params).toEqual({ topic: '' });
  });
});

describe('mapServerRejection', () => {
  const fields = ['topic', 'depth', 'max_cost', 'max_time'];

  it('pins each refusal to its field', () => {
    const rejection = mapServerRejection(
      {
        error: 'the supplied parameters do not satisfy the playbook\'s schema',
        fields: [
          { field: 'topic', message: '"ATTENTION" does not match "^[a-z ]+$"' },
          { field: 'max_cost', message: 'max_cost 90 is above this controller\'s cap of 25' },
        ],
      },
      fields
    );
    expect(rejection.fieldErrors.get('topic')).toBe('"ATTENTION" does not match "^[a-z ]+$"');
    expect(rejection.fieldErrors.get('max_cost')).toBe(
      'max_cost 90 is above this controller\'s cap of 25'
    );
    expect(rejection.general).toBeNull();
  });

  it('keeps a refusal this form has no input for visible', () => {
    const rejection = mapServerRejection(
      {
        error: 'refused',
        fields: [
          { field: 'smuggled', message: 'additional properties are not allowed' },
          { field: '', message: 'the stored params schema is not a usable JSON Schema' },
        ],
      },
      fields
    );
    expect(rejection.fieldErrors.size).toBe(0);
    expect(rejection.general).toBe(
      'smuggled: additional properties are not allowed\nthe stored params schema is not a usable JSON Schema'
    );
  });

  it('falls back to the plain error bodies the rest of the API answers with', () => {
    expect(mapServerRejection({ error: 'no playbook "survey"' }, fields).general).toBe(
      'no playbook "survey"'
    );
    expect(mapServerRejection('boom', fields).general).toBe('boom');
    expect(mapServerRejection(undefined, fields).general).toBe('Unknown error');
  });
});

describe('ceilings', () => {
  it('holds a cost inside the admin cap', () => {
    expect(clampCeiling(90, 25)).toBe(25);
    expect(clampCeiling(4.5, 25)).toBe(4.5);
    expect(clampCeiling(4.5, null)).toBe(4.5);
    expect(clampCeiling(-1, 25)).toBe(0);
    expect(clampCeiling(Number.NaN, 25)).toBe(0);
  });

  it('reads the engine duration grammar and refuses the rest', () => {
    expect(parseMaxTime('90s')).toBe(90);
    expect(parseMaxTime('30m')).toBe(1800);
    expect(parseMaxTime(' 2h ')).toBe(7200);
    expect(parseMaxTime('45')).toBe(45);
    for (const bad of ['', 'garbage', '10x', '-5', '1.5h', '30 m', '0']) {
      expect(parseMaxTime(bad), bad).toBeNull();
    }
  });

  it('bounds a wall-clock ceiling by the cap', () => {
    expect(validateMaxTime('30m', '4h')).toBeNull();
    expect(validateMaxTime('6h', '4h')).toBe("max_time is above this controller's cap of 4h");
    expect(validateMaxTime('nope', '4h')).toBe('max_time takes a duration like 90s, 30m or 2h');
    expect(validateMaxTime('6h', null)).toBeNull();
  });
});

describe('validateCronExpr', () => {
  it('takes the five-field shape and counts what it got', () => {
    expect(validateCronExpr('  0 6 * * MON-FRI ')).toBeNull();
    expect(validateCronExpr('*/15 * * * *')).toBeNull();
    expect(validateCronExpr('')).toBe('cron_expr is required');
    expect(validateCronExpr('0 6 * *')).toBe(
      'cron_expr takes five fields (minute hour day month weekday), not 4'
    );
    expect(validateCronExpr('0 0 6 * * *')).toBe(
      'cron_expr takes five fields (minute hour day month weekday), not 6'
    );
  });

  it('leaves the grammar to the endpoint', () => {
    expect(validateCronExpr('nope nope nope nope nope')).toBeNull();
  });
});

describe('scheduleBody', () => {
  it('posts the launch document plus the recurrence', () => {
    const body = scheduleBody(
      specsOf(FIXTURE),
      { topic: ' attention ', depth: 'deep', since: '', note: '' },
      {
        maxCost: 4.5,
        maxTime: '30m',
        schemaDigest: 'sha256:beef',
        dispatchTarget: 'wharf',
      },
      { playbook: 'survey', cronExpr: ' 0 6 * * MON-FRI ', tz: ' America/New_York ' }
    );
    expect(body).toEqual({
      playbook: 'survey',
      params: { topic: 'attention', depth: 'deep' },
      max_cost: 4.5,
      max_time: '30m',
      schema_digest: 'sha256:beef',
      dispatch_target: 'wharf',
      cron_expr: '0 6 * * MON-FRI',
      tz: 'America/New_York',
    });
  });

  it('names no provider either: a firing launches the same body a run-now does', () => {
    const body = scheduleBody(
      specsOf(FIXTURE),
      { topic: 'attention' },
      { maxCost: 1, maxTime: '30m', schemaDigest: 'sha256:beef' },
      { playbook: 'survey', cronExpr: '0 6 * * MON-FRI', tz: 'UTC' }
    );
    expect('provider' in body).toBe(false);
    expect('model' in body).toBe(false);
  });
});

describe('previewMatches', () => {
  const recurrence = { playbook: 'survey', cronExpr: ' 0 6 * * * ', tz: ' UTC ' };
  const preview = { cron_expr: '0 6 * * *', tz: 'UTC', firings: ['2026-08-24T06:00:00Z'] };

  it('holds the commit until a preview of these inputs exists', () => {
    expect(previewMatches(null, recurrence)).toBe(false);
    expect(previewMatches(preview, recurrence)).toBe(true);
  });

  it('refuses a preview taken against a different expression or zone', () => {
    expect(previewMatches(preview, { ...recurrence, cronExpr: '0 7 * * *' })).toBe(false);
    expect(previewMatches(preview, { ...recurrence, tz: 'America/New_York' })).toBe(false);
  });

  it('counts an expression with no firing ahead as previewed', () => {
    expect(previewMatches({ ...preview, firings: [] }, recurrence)).toBe(true);
  });
});

describe('runSnapshot', () => {
  it('reads a relaunch prefill off the frozen snapshot', () => {
    expect(
      runSnapshot({
        params: { topic: 'attention', depth: 'deep' },
        max_cost: 12,
        max_time: '2h',
      })
    ).toEqual({ values: { topic: 'attention', depth: 'deep' }, maxCost: 12, maxTime: '2h' });
  });

  it('drops anything that is not a string value, and survives an unusable snapshot', () => {
    expect(runSnapshot({ params: { topic: 'attention', n: 3 }, max_cost: 1, max_time: '90s' }).values).toEqual({
      topic: 'attention',
    });
    expect(runSnapshot({ params: null, max_cost: 1, max_time: '90s' }).values).toEqual({});
    expect(runSnapshot({ params: ['topic'], max_cost: 1, max_time: '90s' }).values).toEqual({});
  });

  it('feeds the form, where a snapshot value beats the pack default and an edit beats both', () => {
    const snapshot = runSnapshot({
      params: { topic: 'attention', depth: 'deep' },
      max_cost: 12,
      max_time: '2h',
    });
    const values = initialValues(specsOf(FIXTURE), snapshot.values);
    expect(values).toEqual({ topic: 'attention', depth: 'deep', since: '2026-01-01', note: '' });
    expect(
      launchBody(specsOf(FIXTURE), { ...values, topic: 'kv cache' }, {
        maxCost: snapshot.maxCost,
        maxTime: snapshot.maxTime,
        schemaDigest: 'sha256:beef',
      })
    ).toEqual({
      params: { topic: 'kv cache', depth: 'deep', since: '2026-01-01' },
      max_cost: 12,
      max_time: '2h',
      schema_digest: 'sha256:beef',
    });
  });
});
