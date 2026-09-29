import { useEffect, useRef, useSyncExternalStore } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { readValue, subscribeFlags, writeValue } from './deviceStore';
import { ALL } from './ownerContext';

/// Where this device keeps the masthead owner context.
export const OWNER_CONTEXT_KEY = 'crucible.owner.context';

/// Where this device keeps why the controller last refused the team acted as, until the next pick.
const REFUSED_KEY = 'crucible.owner.refused';

/// The request header naming the team a request acts as. The controller answers every request
/// carrying it for that team, and marks a refusal by echoing it back as `refused`.
export const ACT_AS_HEADER = 'x-crucible-act-as';

/// The team principal requests act as for a stored owner context: the context itself when it names
/// a team, and nothing for `all` or the caller's own user.
export function actAsFor(context: string | null): string | null {
  return context !== null && context.startsWith('team:') ? context : null;
}

/// The team this device's requests act as right now.
export function currentActAs(): string | null {
  return actAsFor(readValue(OWNER_CONTEXT_KEY));
}

/// The headers a plain `fetch` adds so it acts as the same team the typed client does.
export function actAsHeaders(): Record<string, string> {
  const team = currentActAs();
  return team === null ? {} : { [ACT_AS_HEADER]: team };
}

/// Whether a response says the controller refused the team the request acted as.
export function actAsRefused(response: Response): boolean {
  return response.headers.get(ACT_AS_HEADER) === 'refused';
}

/// The reason a refusal carries, verbatim when its body says one.
export async function refusalReason(response: Response): Promise<string> {
  try {
    const body: unknown = await response.clone().json();
    if (typeof body === 'object' && body !== null && 'error' in body && typeof body.error === 'string') {
      return body.error;
    }
  } catch {
    // Not JSON: fall through to the generic reason.
  }
  return `the controller refused to act as that team (${response.status})`;
}

/// Drop back to every owner after the controller refused the team acted as, so the next requests
/// answer for the caller instead of failing forever, and keep the reason to show.
export function forgetActAs(reason: string): void {
  writeValue(REFUSED_KEY, reason);
  writeValue(OWNER_CONTEXT_KEY, ALL);
}

/// Why the controller last refused the team acted as, until the caller picks a context again.
export function actAsRefusal(): string | null {
  const reason = readValue(REFUSED_KEY);
  return reason === null || reason === '' ? null : reason;
}

export function clearActAsRefusal(): void {
  writeValue(REFUSED_KEY, '');
}

/// Every cached answer was made for one subject, so a change of the team acted as drops them all.
/// Mount once, at the root.
export function useResetOnActAs(): void {
  const qc = useQueryClient();
  const team = useSyncExternalStore(subscribeFlags, currentActAs, () => null);
  const last = useRef(team);
  useEffect(() => {
    if (last.current === team) return;
    last.current = team;
    void qc.resetQueries();
  }, [team, qc]);
}
