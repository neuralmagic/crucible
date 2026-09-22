// Per-device UI state: pane layouts and the rail's collapsed flag. These are viewport ergonomics
// rather than identity preferences, so they stay in this browser's localStorage instead of the
// server-side editor-prefs document a 13-inch laptop and an ultrawide would otherwise fight over.

export interface DeviceStorage {
  getItem: (key: string) => string | null;
  setItem: (key: string, value: string) => void;
}

const memory = new Map<string, string>();

function backing(): DeviceStorage | null {
  try {
    const store = globalThis.localStorage;
    return store === undefined || store === null ? null : store;
  } catch {
    return null;
  }
}

/// localStorage where the browser allows it, an in-memory map where it does not: a private window
/// that throws on access still gets working panes for the life of the tab.
export const deviceStorage: DeviceStorage = {
  getItem(key) {
    const store = backing();
    if (store === null) return memory.get(key) ?? null;
    try {
      return store.getItem(key);
    } catch {
      return memory.get(key) ?? null;
    }
  },
  setItem(key, value) {
    memory.set(key, value);
    const store = backing();
    if (store === null) return;
    try {
      store.setItem(key, value);
    } catch {
      // The memory map above already holds it.
    }
  },
};

const flagListeners = new Set<() => void>();

/// A flag can be written from two places at once (the hint's own dismiss button and the Display
/// menu's toggle), so every reader subscribes rather than holding its own copy of the value.
export function subscribeFlags(listener: () => void): () => void {
  flagListeners.add(listener);
  return () => {
    flagListeners.delete(listener);
  };
}

export function readFlag(key: string, fallback: boolean): boolean {
  const stored = deviceStorage.getItem(key);
  if (stored === '1') return true;
  if (stored === '0') return false;
  return fallback;
}

export function writeFlag(key: string, value: boolean): void {
  deviceStorage.setItem(key, value ? '1' : '0');
  for (const listener of flagListeners) listener();
}

export function readValue(key: string): string | null {
  return deviceStorage.getItem(key);
}

export function writeValue(key: string, value: string): void {
  deviceStorage.setItem(key, value);
  for (const listener of flagListeners) listener();
}
