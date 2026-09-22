import { useCallback, useMemo, useSyncExternalStore } from 'react';
import { $api } from './api/client';
import { readValue, subscribeFlags, writeValue } from './deviceStore';
import { ALL, actedAs, effectiveContext, ownerOptions, switchable, type ActedAs } from './ownerContext';

const KEY = 'crucible.owner.context';

export interface OwnerContext {
  /// `all` or the principal whose resources the lists show.
  context: string;
  setContext: (next: string) => void;
  /// Every principal the caller acts as.
  principals: ActedAs[];
  /// The principals the switcher offers.
  switchable: ActedAs[];
  /// The principals a creation form may own as.
  owners: ActedAs[];
  /// Whether the session has answered; before that the lists show everything.
  ready: boolean;
}

/// The owner context every list and creation form follows, per device.
export function useOwnerContext(): OwnerContext {
  const whoami = $api.useQuery('get', '/api/whoami');
  const stored = useSyncExternalStore(subscribeFlags, () => readValue(KEY), () => null);
  const principals = useMemo(() => actedAs(whoami.data), [whoami.data]);
  const context = whoami.isSuccess ? effectiveContext(stored, principals) : ALL;
  const setContext = useCallback((next: string) => {
    writeValue(KEY, next);
  }, []);
  return {
    context,
    setContext,
    principals,
    switchable: useMemo(() => switchable(principals), [principals]),
    owners: useMemo(() => ownerOptions(principals), [principals]),
    ready: whoami.isSuccess,
  };
}
