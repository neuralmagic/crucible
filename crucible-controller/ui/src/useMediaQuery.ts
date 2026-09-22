import { useCallback, useSyncExternalStore } from 'react';

/// Whether the viewport matches, tracked as it changes. A layout that only makes sense wide, such
/// as a two-pane split, asks this rather than guessing from a breakpoint class it cannot read.
export function useMediaQuery(query: string): boolean {
  const subscribe = useCallback(
    (notify: () => void) => {
      const list = window.matchMedia(query);
      list.addEventListener('change', notify);
      return () => {
        list.removeEventListener('change', notify);
      };
    },
    [query]
  );

  return useSyncExternalStore(
    subscribe,
    () => window.matchMedia(query).matches,
    () => false
  );
}
