import { useEffect, useRef, useState } from 'react';
import type { ConnState } from './useLiveSession';
import {
  appendLog,
  parseActivity,
  parseProgress,
  upsertBeat,
  type LogLine,
  type ScopeActivityBeat,
  type ScopeProgressBeat,
} from './turnStream';
import { openEventSource } from '../api/session';

export interface TurnLiveState {
  /// The pod's latest observed phase (Pending/Running/Succeeded/Failed), null before the first event.
  phase: string | null;
  /// The per-round progress rail, deduped by (round, kind).
  beats: ScopeProgressBeat[];
  /// The latest within-round activity beat — the ticker shows only the newest one.
  activity: ScopeActivityBeat | null;
  /// The capped log scrollback.
  logs: LogLine[];
  connState: ConnState;
}

/// Client-side log cap: enough scrollback to be useful, small enough to never bog the tab.
const LOG_CAP = 500;
/// Coalesce streamed lines into one render per tick (same discipline as useLiveSession).
const FLUSH_MS = 120;

/// Subscribe to `GET /api/turns/:podName/live` (native EventSource — same-origin, the oauth2-proxy
/// cookie rides along like every /api call). The stream has no seq/resume: on reconnect the server
/// replays the pod's logs from the start, so buffers reset on every (re)open — beats dedup by
/// (round, kind) and logs just refill. Terminal `end` reasons: turn-not-running, pod-gone,
/// completed: <phase>, timeout, error: … A 404 (unknown pod) surfaces as EventSource retrying
/// forever, so callers should only mount this with a pod name they got from the turns ledger.
export function useTurnLive(podName: string): TurnLiveState {
  const [phase, setPhase] = useState<string | null>(null);
  const [beats, setBeats] = useState<ScopeProgressBeat[]>([]);
  const [activity, setActivity] = useState<ScopeActivityBeat | null>(null);
  const [logs, setLogs] = useState<LogLine[]>([]);
  const [connState, setConnState] = useState<ConnState>({ state: 'connecting' });

  // Monotonic log-line ids across flushes (React keys).
  const nextId = useRef(0);

  useEffect(() => {
    setPhase(null);
    setBeats([]);
    setActivity(null);
    setLogs([]);
    setConnState({ state: 'connecting' });

    const source = openEventSource(`/api/turns/${encodeURIComponent(podName)}/live`);
    const pending: string[] = [];

    const flush = () => {
      if (pending.length === 0) return;
      const batch = pending.splice(0, pending.length);
      setLogs((prev) => {
        let acc = prev;
        for (const text of batch) {
          nextId.current += 1;
          acc = appendLog(acc, nextId.current, text, LOG_CAP);
        }
        return acc;
      });
    };
    const timer = window.setInterval(flush, FLUSH_MS);

    const markLive = () => setConnState((s) => (s.state === 'ended' ? s : { state: 'live' }));

    source.addEventListener('phase', (event) => {
      if (!(event instanceof MessageEvent) || typeof event.data !== 'string') return;
      setPhase(event.data);
      markLive();
    });

    source.addEventListener('progress', (event) => {
      if (!(event instanceof MessageEvent) || typeof event.data !== 'string') return;
      const beat = parseProgress(event.data);
      if (beat) setBeats((prev) => upsertBeat(prev, beat));
      markLive();
    });

    source.addEventListener('activity', (event) => {
      if (!(event instanceof MessageEvent) || typeof event.data !== 'string') return;
      const beat = parseActivity(event.data);
      // A reconnect replays the stream, so "latest wins" self-heals — no dedup needed.
      if (beat) setActivity(beat);
      markLive();
    });

    source.addEventListener('log', (event) => {
      if (!(event instanceof MessageEvent) || typeof event.data !== 'string') return;
      pending.push(event.data);
      markLive();
    });

    source.addEventListener('end', (event) => {
      const reason =
        event instanceof MessageEvent && typeof event.data === 'string' && event.data
          ? event.data
          : 'ended';
      flush();
      source.close();
      window.clearInterval(timer);
      setConnState({ state: 'ended', reason });
    });

    source.onopen = () => {
      // A reconnect replays the whole log stream — start the buffer over rather than duplicating.
      pending.length = 0;
      setLogs([]);
      markLive();
    };

    source.onerror = () => {
      if (source.readyState === EventSource.CONNECTING) {
        setConnState((s) => (s.state === 'ended' ? s : { state: 'reconnecting' }));
      }
    };

    return () => {
      source.close();
      window.clearInterval(timer);
    };
  }, [podName]);

  return { phase, beats, activity, logs, connState };
}
