import { useEffect, useRef, useState } from 'react';
import { parseSessionLine, parseStatus, type LiveStatus, type SessionLine } from './session';
import { appendLine, appendPodLine, type FeedRow } from './feed';
import { openEventSource } from '../api/session';

/// The relay's connection lifecycle, as the pane shows it. `reconnecting` is EventSource's native
/// retry (Last-Event-ID resumes server-side); `ended` is terminal — the server sent an `end` event.
export type ConnState =
  | { state: 'connecting' }
  | { state: 'live' }
  | { state: 'reconnecting' }
  | { state: 'ended'; reason: string };

export interface LiveSessionState {
  status: LiveStatus | null;
  connState: ConnState;
  rows: FeedRow[];
  /// The pod's phase, sent only by the spoke relay (a hub run carries richer status snapshots
  /// instead). Null on a hub run, and until the first phase arrives on a spoke one.
  phase: string | null;
}

/// Cap the in-memory feed; oldest rows drop past this.
const ROW_CAP = 2000;
/// Coalesce a burst of streamed lines into one render this often, so token-rate agent text doesn't
/// drive a re-render per delta.
const FLUSH_MS = 120;

/// Subscribe to `GET /api/runs/:runId/live` (native EventSource — same-origin, oauth2-proxy cookie
/// rides along like every /api call). Returns the latest status snapshot, the connection state, and
/// the capped, coalesced feed. `onEnded` fires once with the terminal reason (run-not-running /
/// pod-gone / bridge-closed / error: …). EventSource auto-reconnects on transient drops, replaying
/// from the last seq via Last-Event-ID; we only stop on an explicit `end`.
export function useLiveSession(runId: string, onEnded?: (reason: string) => void): LiveSessionState {
  const [status, setStatus] = useState<LiveStatus | null>(null);
  const [connState, setConnState] = useState<ConnState>({ state: 'connecting' });
  const [rows, setRows] = useState<FeedRow[]>([]);
  const [phase, setPhase] = useState<string | null>(null);
  const onEndedRef = useRef(onEnded);
  onEndedRef.current = onEnded;

  useEffect(() => {
    setStatus(null);
    setRows([]);
    setPhase(null);
    setConnState({ state: 'connecting' });

    const source = openEventSource(`/api/runs/${encodeURIComponent(runId)}/live`);
    const pending: { id: number; line: SessionLine }[] = [];
    let rowId = 0;

    const flush = () => {
      if (pending.length === 0) return;
      const batch = pending.splice(0, pending.length);
      setRows((prev) => {
        let acc = prev;
        for (const p of batch) acc = appendLine(acc, p.id, p.line, ROW_CAP);
        return acc;
      });
    };
    const timer = window.setInterval(flush, FLUSH_MS);

    // A live event means the stream is flowing again; never clobber a terminal `ended`.
    const markLive = () => setConnState((s) => (s.state === 'ended' ? s : { state: 'live' }));

    source.addEventListener('session', (event) => {
      if (!(event instanceof MessageEvent) || typeof event.data !== 'string') return;
      rowId += 1;
      pending.push({ id: rowId, line: parseSessionLine(event.data) });
      markLive();
    });

    // A run dispatched to a spoke cannot be reached over the control bridge, so the relay streams
    // its pod instead: phase on connect and change, log lines until the run's session NDJSON
    // arrives, and session events from there on. The two shapes share this one subscription.
    source.addEventListener('phase', (event) => {
      if (!(event instanceof MessageEvent) || typeof event.data !== 'string') return;
      setPhase(event.data);
      markLive();
    });

    source.addEventListener('log', (event) => {
      if (!(event instanceof MessageEvent) || typeof event.data !== 'string') return;
      rowId += 1;
      const id = rowId;
      const text = event.data;
      setRows((prev) => appendPodLine(prev, id, text, ROW_CAP));
      markLive();
    });

    source.addEventListener('status', (event) => {
      if (!(event instanceof MessageEvent) || typeof event.data !== 'string') return;
      const snapshot = parseStatus(event.data);
      if (snapshot) setStatus(snapshot);
      markLive();
    });

    source.addEventListener('end', (event) => {
      const reason = event instanceof MessageEvent && typeof event.data === 'string' && event.data ? event.data : 'ended';
      flush();
      source.close();
      window.clearInterval(timer);
      setConnState({ state: 'ended', reason });
      onEndedRef.current?.(reason);
    });

    source.onopen = markLive;

    source.onerror = () => {
      // EventSource retries on its own (readyState CONNECTING). If we already closed it on `end`,
      // readyState is CLOSED and there's nothing to reconnect.
      if (source.readyState === EventSource.CONNECTING) {
        setConnState((s) => (s.state === 'ended' ? s : { state: 'reconnecting' }));
      }
    };

    return () => {
      source.close();
      window.clearInterval(timer);
    };
  }, [runId]);

  return { status, connState, rows, phase };
}
