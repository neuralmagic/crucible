// The option shape a pick list renders and the ordering behind a filterable one: what a filter
// keeps, and where the starred entries sit. Pure so the arrangement is testable without a field.

import type { ActedAs } from '../ownerContext';

export interface PickOption {
  value: string;
  label: string;
}

export interface ArrangedOption extends PickOption {
  favorite: boolean;
}

/// Every principal the signed-in caller may own a resource as, as a select takes them. Empty
/// when nobody is signed in, which is what disables the form.
export function principalOptions(owners: readonly ActedAs[]): PickOption[] {
  return owners.map((o) => ({
    value: o.value,
    label: o.kind === 'team' ? `${o.value} (${o.role})` : o.value,
  }));
}

/// The options a filter keeps, starred ones first. Matching is a case-insensitive substring of the
/// label or the value, so `team-x` finds `group:/groups/team-x`. Within each half the caller's order
/// is kept, which is the order the session listed the principals in.
export function arrangeOptions(
  options: readonly PickOption[],
  filter: string,
  favorites: readonly string[],
): ArrangedOption[] {
  const needle = filter.trim().toLowerCase();
  const starred = new Set(favorites);
  const kept = options
    .filter(
      (option) =>
        needle.length === 0 ||
        option.label.toLowerCase().includes(needle) ||
        option.value.toLowerCase().includes(needle),
    )
    .map((option) => ({ ...option, favorite: starred.has(option.value) }));
  return [...kept.filter((o) => o.favorite), ...kept.filter((o) => !o.favorite)];
}

/// Narrows a select's string back into the option table's own domain without a cast.
export function optionValue<T extends string>(options: readonly { value: T }[], raw: string, fallback: T): T {
  return options.find((o) => o.value === raw)?.value ?? fallback;
}

/// Favorites with `value` toggled, order kept for the rest.
export function toggleFavorite(favorites: readonly string[], value: string): string[] {
  return favorites.includes(value)
    ? favorites.filter((favorite) => favorite !== value)
    : [...favorites, value];
}
