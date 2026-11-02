import { useCallback, useMemo } from 'react';
import { $api } from './api/client';
import { usePickerPrefs } from './api/pickerPrefs';
import { toggleFavorite } from './pages/pickList';
import { activeTeams } from './teamPicks';

/// The teams the caller starred, and a toggle for one of them. A team star is the same star the
/// owner picker keeps for `team:<slug>`, stored against the user.
export function useStarredTeams(): [string[], (slug: string) => void] {
  const { prefs, setPref } = usePickerPrefs();
  const favorites = prefs.ownerFavorites;
  const starred = useMemo(
    () => favorites.filter((owner) => owner.startsWith('team:')).map((owner) => owner.slice('team:'.length)),
    [favorites],
  );
  const toggle = useCallback(
    (slug: string) => {
      setPref('ownerFavorites', toggleFavorite(favorites, `team:${slug}`));
    },
    [favorites, setPref],
  );
  return [starred, toggle];
}

/// The teams that own a playbook, a draft, or a schedule.
export function useActiveTeams(): ReadonlySet<string> {
  const playbooks = $api.useQuery('get', '/api/playbooks');
  const drafts = $api.useQuery('get', '/api/playbook-drafts');
  const schedules = $api.useQuery('get', '/api/schedules');
  return useMemo(
    () =>
      activeTeams([
        ...(playbooks.data ?? []).map((p) => p.owner),
        ...(drafts.data ?? []).map((d) => d.owner),
        ...(schedules.data ?? []).map((s) => s.owner_principal),
      ]),
    [playbooks.data, drafts.data, schedules.data],
  );
}
