import { useCallback } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { $api } from './client';

/**
 * Per-user display settings. The server stores the blob free-form and enforces only a size cap, so
 * the shape is the client's to declare and to validate on the way in.
 */
export interface Prefs {
  /** Phosphor post-process on the dark-theme charts. */
  chartCrt: boolean;
}

export const PREFS_DEFAULTS: Prefs = {
  chartCrt: true,
};

/** Keys the server round-trips but this client does not own are preserved on write. */
type StoredPrefs = Record<string, unknown>;

function parse(stored: StoredPrefs | undefined): Prefs {
  const raw = stored?.chartCrt;
  return {
    chartCrt: typeof raw === 'boolean' ? raw : PREFS_DEFAULTS.chartCrt,
  };
}

export function usePrefs(): {
  prefs: Prefs;
  setPref: <K extends keyof Prefs>(key: K, value: Prefs[K]) => void;
} {
  const queryClient = useQueryClient();
  const query = $api.useQuery('get', '/api/prefs');
  const mutation = $api.useMutation('put', '/api/prefs');

  const stored: StoredPrefs | undefined = query.data?.prefs;
  const prefs = parse(stored);

  const setPref = useCallback(
    <K extends keyof Prefs>(key: K, value: Prefs[K]) => {
      const next: StoredPrefs = { ...stored, [key]: value };
      mutation.mutate(
        { body: { prefs: next } },
        {
          onSettled: () => {
            void queryClient.invalidateQueries({ queryKey: ['get', '/api/prefs'] });
          },
        },
      );
    },
    [stored, mutation, queryClient],
  );

  return { prefs, setPref };
}
