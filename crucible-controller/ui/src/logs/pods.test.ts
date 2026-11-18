import { describe, expect, it } from 'vitest';
import { filterByPod, podPrefixOf, podPrefixes } from './pods';

const WORKLOAD = [
  '[pod/workload-7f9f-runner/runner] submitting job/overlay-p1',
  '[pod/workload-7f9f-runner/runner] waiting',
  '[pod/workload-7f9f-pre0/pre] pulling image',
  '[pod/workload-7f9f-runner/runner] FATAL: job/overlay-p1 failed',
].join('\n');

describe('podPrefixOf', () => {
  it('reads the kubectl log prefix with and without a container', () => {
    expect(podPrefixOf('[pod/workload-7f9f-runner/runner] waiting')).toEqual({
      pod: 'workload-7f9f-runner',
      container: 'runner',
    });
    expect(podPrefixOf('[pod/workload-7f9f-pre0] ready')).toEqual({
      pod: 'workload-7f9f-pre0',
      container: null,
    });
  });

  it('finds a prefix behind ANSI colour escapes', () => {
    expect(podPrefixOf('\x1b[31m[pod/workload-7f9f-runner/runner] failed\x1b[0m')?.pod).toBe(
      'workload-7f9f-runner'
    );
  });

  it('is null for an unprefixed engine line', () => {
    expect(podPrefixOf('18:02:11 INFO dispatching')).toBeNull();
  });

  it('ignores a mention that is not the line prefix', () => {
    expect(podPrefixOf('see [pod/workload-7f9f-runner/runner] above')).toBeNull();
  });
});

describe('podPrefixes', () => {
  it('counts the lines each pod prefixed in first-seen order', () => {
    expect(podPrefixes(WORKLOAD)).toEqual([
      { pod: 'workload-7f9f-runner', lines: 3 },
      { pod: 'workload-7f9f-pre0', lines: 1 },
    ]);
  });

  it('is empty for output no pod prefixed', () => {
    expect(podPrefixes('plain engine output\nno pods here')).toEqual([]);
  });
});

describe('filterByPod', () => {
  it('keeps only the chosen pod and drops the rest', () => {
    expect(filterByPod(WORKLOAD, 'workload-7f9f-pre0')).toBe(
      '[pod/workload-7f9f-pre0/pre] pulling image'
    );
  });

  it('keeps everything when no pod is chosen', () => {
    expect(filterByPod(WORKLOAD, null)).toBe(WORKLOAD);
  });
});
