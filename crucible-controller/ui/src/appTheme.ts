import { useSyncExternalStore } from 'react';

export type AppTheme = 'dark' | 'light';

export type AppBrand = 'crucible' | 'redhat';

export const APP_BRANDS = ['crucible', 'redhat'] as const satisfies readonly AppBrand[];

export function isAppBrand(value: unknown): value is AppBrand {
  return APP_BRANDS.some((brand) => brand === value);
}

/// `DisplayPrefs` writes the attribute and index.html sets it pre-paint, so the attribute is the
/// source of truth, not a second copy.
export function isDarkTheme(): boolean {
  return document.documentElement.dataset.theme !== 'light';
}

function currentTheme(): AppTheme {
  return isDarkTheme() ? 'dark' : 'light';
}

export function currentBrand(): AppBrand {
  const brand = document.documentElement.dataset.brand;
  return isAppBrand(brand) ? brand : 'crucible';
}

export function subscribeTheme(listener: () => void): () => void {
  const observer = new MutationObserver(listener);
  observer.observe(document.documentElement, {
    attributes: true,
    attributeFilter: ['class', 'data-theme', 'data-brand'],
  });
  return () => observer.disconnect();
}

export function useAppTheme(): AppTheme {
  return useSyncExternalStore(subscribeTheme, currentTheme);
}

export function useAppBrand(): AppBrand {
  return useSyncExternalStore(subscribeTheme, currentBrand);
}
