import { describe, expect, it } from 'vitest';
import { diffIsEmpty, diffParamsSchemas } from './schemaDiff';

function schema(properties: Record<string, unknown>, required: string[] = []): unknown {
  return { type: 'object', properties, required };
}

describe('diffParamsSchemas', () => {
  it('reports nothing when the form is unchanged', () => {
    const one = schema({ topic: { type: 'string', pattern: '^[a-z]+$' } }, ['topic']);
    const diff = diffParamsSchemas(one, schema({ topic: { type: 'string', pattern: '^[a-z]+$' } }, ['topic']));
    expect(diff.comparable).toBe(true);
    expect(diffIsEmpty(diff)).toBe(true);
  });

  it('names the params a bump adds and drops', () => {
    const diff = diffParamsSchemas(
      schema({ topic: { type: 'string' }, depth: { type: 'string' } }),
      schema({ topic: { type: 'string' }, limit: { type: 'string' } })
    );
    expect(diff.added.map((s) => s.name)).toEqual(['limit']);
    expect(diff.removed.map((s) => s.name)).toEqual(['depth']);
    expect(diff.changed).toEqual([]);
  });

  it('names every property that moved on a param that stayed', () => {
    const diff = diffParamsSchemas(
      schema({ topic: { type: 'string', pattern: '^[a-z]+$', default: 'a', description: 'old' } }),
      schema({ topic: { type: 'string', pattern: '^[A-Z]+$', default: 'b', description: 'new' } }, [
        'topic',
      ])
    );
    expect(diff.changed).toEqual([
      { name: 'topic', field: 'required', from: 'optional', to: 'required' },
      { name: 'topic', field: 'pattern', from: '^[a-z]+$', to: '^[A-Z]+$' },
      { name: 'topic', field: 'default', from: 'a', to: 'b' },
      { name: 'topic', field: 'description', from: 'old', to: 'new' },
    ]);
    expect(diffIsEmpty(diff)).toBe(false);
  });

  it('reports a flipped requirement on its own', () => {
    const diff = diffParamsSchemas(
      schema({ topic: { type: 'string' } }, ['topic']),
      schema({ topic: { type: 'string' } })
    );
    expect(diff.changed).toEqual([
      { name: 'topic', field: 'required', from: 'required', to: 'optional' },
    ]);
  });

  it('reports a dropped pattern as a change to none', () => {
    const diff = diffParamsSchemas(
      schema({ topic: { type: 'string', pattern: '^x$' } }),
      schema({ topic: { type: 'string' } })
    );
    expect(diff.changed).toEqual([
      { name: 'topic', field: 'pattern', from: '^x$', to: null },
    ]);
  });

  /// A schema the form cannot render cannot be diffed either, and saying "no change" about one
  /// would be a lie the importer would act on.
  it('refuses to compare a schema outside the rendered subset', () => {
    const diff = diffParamsSchemas(schema({ topic: { type: 'number' } }), schema({}));
    expect(diff.comparable).toBe(false);
    expect(diffIsEmpty(diff)).toBe(true);

    expect(diffParamsSchemas(undefined, schema({})).comparable).toBe(false);
    expect(diffParamsSchemas(schema({}), null).comparable).toBe(false);
  });
});
