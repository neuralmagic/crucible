import { useCallback } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { $api } from './client';

/**
 * What the pickers remember per user: starred entries and the last filter. Stored against the
 * caller's identity (`/api/prefs/pickers`), so a fresh browser reads it back; the server keeps
 * the blob opaque and the shape is validated here on the way in.
 */
export interface PickerPrefs {
  /** Principals starred in the secret owner picker, in the order they were starred. */
  ownerFavorites: string[];
  /** The filter text the secret owner picker was last left with. */
  ownerFilter: string;
}

export const PICKER_PREFS_DEFAULTS: PickerPrefs = {
  ownerFavorites: [],
  ownerFilter: '',
};

const QUERY_KEY = ['get', '/api/prefs/pickers'];

type Stored = Record<string, unknown>;

function strings(raw: unknown): string[] | null {
  if (!Array.isArray(raw)) return null;
  return raw.filter((item): item is string => typeof item === 'string');
}

export function parsePickerPrefs(stored: Stored | undefined): PickerPrefs {
  const filter = stored?.ownerFilter;
  return {
    ownerFavorites: strings(stored?.ownerFavorites) ?? PICKER_PREFS_DEFAULTS.ownerFavorites,
    ownerFilter: typeof filter === 'string' ? filter : PICKER_PREFS_DEFAULTS.ownerFilter,
  };
}

export function usePickerPrefs(): {
  prefs: PickerPrefs;
  setPref: <K extends keyof PickerPrefs>(key: K, value: PickerPrefs[K]) => void;
} {
  const queryClient = useQueryClient();
  const query = $api.useQuery('get', '/api/prefs/pickers');
  const mutation = $api.useMutation('put', '/api/prefs/pickers');

  const stored: Stored | undefined = query.data?.prefs;
  const prefs = parsePickerPrefs(stored);

  const setPref = useCallback(
    <K extends keyof PickerPrefs>(key: K, value: PickerPrefs[K]) => {
      const next: Stored = { ...stored, [key]: value };
      mutation.mutate(
        { body: { prefs: next } },
        {
          onSettled: () => {
            void queryClient.invalidateQueries({ queryKey: QUERY_KEY });
          },
        },
      );
    },
    [stored, mutation, queryClient],
  );

  return { prefs, setPref };
}
