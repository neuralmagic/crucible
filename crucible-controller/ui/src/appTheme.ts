import { useSyncExternalStore } from 'react';

export type AppTheme = 'dark' | 'light';

/// `DisplayPrefs` writes the attribute and index.html sets it pre-paint, so the attribute is the
/// source of truth, not a second copy.
export function isDarkTheme(): boolean {
  return document.documentElement.dataset.theme !== 'light';
}

function currentTheme(): AppTheme {
  return isDarkTheme() ? 'dark' : 'light';
}

export function subscribeTheme(listener: () => void): () => void {
  const observer = new MutationObserver(listener);
  observer.observe(document.documentElement, {
    attributes: true,
    attributeFilter: ['class', 'data-theme'],
  });
  return () => observer.disconnect();
}

export function useAppTheme(): AppTheme {
  return useSyncExternalStore(subscribeTheme, currentTheme);
}
