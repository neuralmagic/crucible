import { useCallback, useSyncExternalStore } from 'react';
import { readFlag, subscribeFlags, writeFlag } from './deviceStore';

/// A per-device boolean every holder of the key sees the same value of.
export function useDeviceFlag(key: string, fallback: boolean): [boolean, (value: boolean) => void] {
  const value = useSyncExternalStore(
    subscribeFlags,
    () => readFlag(key, fallback),
    () => fallback,
  );
  const set = useCallback(
    (next: boolean) => {
      writeFlag(key, next);
    },
    [key],
  );
  return [value, set];
}
