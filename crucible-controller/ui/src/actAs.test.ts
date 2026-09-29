import { beforeEach, describe, expect, it } from 'vitest';
import {
  ACT_AS_HEADER,
  OWNER_CONTEXT_KEY,
  actAsFor,
  actAsHeaders,
  actAsRefusal,
  actAsRefused,
  clearActAsRefusal,
  currentActAs,
  forgetActAs,
  refusalReason,
} from './actAs';
import { readValue, subscribeFlags, writeValue } from './deviceStore';

describe('actAs', () => {
  beforeEach(() => {
    writeValue(OWNER_CONTEXT_KEY, 'all');
  });

  it('acts as a team context and nothing else', () => {
    expect(actAsFor('team:llm-d')).toBe('team:llm-d');
    expect(actAsFor('all')).toBeNull();
    expect(actAsFor('user:reed')).toBeNull();
    expect(actAsFor('group:/groups/x')).toBeNull();
    expect(actAsFor(null)).toBeNull();
  });

  it('adds the header only while the stored context names a team', () => {
    expect(currentActAs()).toBeNull();
    expect(actAsHeaders()).toEqual({});
    writeValue(OWNER_CONTEXT_KEY, 'user:reed');
    expect(actAsHeaders()).toEqual({});
    writeValue(OWNER_CONTEXT_KEY, 'team:llm-d');
    expect(currentActAs()).toBe('team:llm-d');
    expect(actAsHeaders()).toEqual({ [ACT_AS_HEADER]: 'team:llm-d' });
  });

  it('reads a refusal only from the marker header', () => {
    expect(actAsRefused(new Response(null, { status: 403, headers: { [ACT_AS_HEADER]: 'refused' } }))).toBe(true);
    expect(actAsRefused(new Response(null, { status: 403 }))).toBe(false);
    expect(actAsRefused(new Response(null, { status: 200, headers: { [ACT_AS_HEADER]: 'team:llm-d' } }))).toBe(false);
  });

  it('forgetting drops back to every owner, keeps the reason, and tells subscribers', () => {
    writeValue(OWNER_CONTEXT_KEY, 'team:llm-d');
    let told = 0;
    const unsubscribe = subscribeFlags(() => {
      told += 1;
    });
    forgetActAs('you are not a member of team:llm-d');
    unsubscribe();
    expect(readValue(OWNER_CONTEXT_KEY)).toBe('all');
    expect(currentActAs()).toBeNull();
    expect(actAsRefusal()).toBe('you are not a member of team:llm-d');
    expect(told).toBe(2);
    clearActAsRefusal();
    expect(actAsRefusal()).toBeNull();
  });

  it('reads the refusal reason verbatim from the body, with a fallback', async () => {
    const json = new Response(JSON.stringify({ error: 'you are not a member of team:ghost' }), { status: 403 });
    expect(await refusalReason(json)).toBe('you are not a member of team:ghost');
    expect(json.bodyUsed).toBe(false);
    expect(await refusalReason(new Response('nope', { status: 403 }))).toContain('403');
    expect(await refusalReason(new Response(JSON.stringify({ message: 'x' }), { status: 403 }))).toContain('403');
  });
});
