import { afterEach, describe, expect, it, vi } from 'vitest';
import { deviceStorage, readFlag, writeFlag } from './deviceStore';

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('per-device storage', () => {
  it('reads back what it wrote through the browser store', () => {
    const held = new Map<string, string>();
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => held.get(key) ?? null,
      setItem: (key: string, value: string) => {
        held.set(key, value);
      },
    });

    deviceStorage.setItem('pane.a', '{"left":40}');
    expect(held.get('pane.a')).toBe('{"left":40}');
    expect(deviceStorage.getItem('pane.a')).toBe('{"left":40}');
  });

  it('keeps working where the browser refuses to store anything', () => {
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new Error('denied');
      },
      setItem: () => {
        throw new Error('denied');
      },
    });

    deviceStorage.setItem('pane.b', '{"left":60}');
    expect(deviceStorage.getItem('pane.b')).toBe('{"left":60}');
    expect(deviceStorage.getItem('pane.never-written')).toBeNull();
  });
});

describe('a per-device flag', () => {
  it('round-trips, and falls back where nothing was stored', () => {
    const held = new Map<string, string>();
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => held.get(key) ?? null,
      setItem: (key: string, value: string) => {
        held.set(key, value);
      },
    });

    expect(readFlag('rail.c', true)).toBe(true);
    writeFlag('rail.c', false);
    expect(readFlag('rail.c', true)).toBe(false);
    writeFlag('rail.c', true);
    expect(readFlag('rail.c', false)).toBe(true);
  });

  it('ignores a stored value it does not recognise', () => {
    vi.stubGlobal('localStorage', {
      getItem: () => 'yes please',
      setItem: () => undefined,
    });

    expect(readFlag('rail.d', false)).toBe(false);
    expect(readFlag('rail.d', true)).toBe(true);
  });
});
