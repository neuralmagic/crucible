/// Expired-session handling, centralized. When the session lapses, API calls come back as a 401
/// (native mode, and the proxy's api-routes) or — from a proxy without that config — as a followed
/// 302 onto a sign-in page, whose HTML must never reach a component. Either way the only useful
/// move is to send the browser through the sign-in flow and back to where it was.
///
/// The sign-in URL is the controller's own `/auth/login` in both auth modes: in proxy mode it
/// redirects to the sidecar's `/oauth2/start`, so the SPA holds one set of URLs across the flip.

export function redirectToSignIn(): void {
  const here = window.location.pathname + window.location.search + window.location.hash;
  window.location.assign(`/auth/login?rd=${encodeURIComponent(here)}`);
}

/// A response that means "session gone": a 401, or a fetch that silently followed the sign-in
/// redirect (the final URL lands under /oauth2/ or /auth/).
export function isSessionExpired(response: Response): boolean {
  if (response.status === 401) return true;
  if (!response.redirected) return false;
  const path = new URL(response.url).pathname;
  return path.startsWith('/oauth2/') || path.startsWith('/auth/');
}

let probe: Promise<void> | null = null;

// EventSource exposes no status code on failure, so streams probe the session with a guarded read
// instead (200 = live session, 401 = expired; redirect: manual so an opaqueredirect also reads as
// expired). Concurrent stream errors share one in-flight probe.
function redirectIfSessionExpired(): Promise<void> {
  probe ??= fetch('/api/whoami', { redirect: 'manual' })
    .then((res) => {
      if (res.status === 401 || res.type === 'opaqueredirect') redirectToSignIn();
    })
    .catch(() => undefined)
    .finally(() => {
      probe = null;
    });
  return probe;
}

/// An EventSource that, on error, checks whether the session expired and redirects to sign-in
/// instead of letting the native reconnect spin forever. Callers attach their own listeners as
/// usual.
export function openEventSource(url: string): EventSource {
  const source = new EventSource(url);
  source.addEventListener('error', () => {
    void redirectIfSessionExpired();
  });
  return source;
}
