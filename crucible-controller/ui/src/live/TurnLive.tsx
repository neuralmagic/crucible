import { useLayoutEffect, useState } from 'react';
import { Button, Mono, Status, statusTone } from '../ui';
import { useTurnLive } from './useTurnLive';
import { connLabel } from './connLabel';
import {
  activityLabel,
  phaseColor,
  type LogLine,
  type ScopeActivityBeat,
  type ScopeProgressBeat,
} from './turnStream';
import { useFollowScroll } from './useFollowScroll';
import { turnDuration } from '../pages/turns';

interface TurnLiveProps {
  podName: string;
  /// The work-pod row's created_at, for the elapsed readout. Optional — no stamp, no readout.
  createdAt?: string | null;
}

/// The live pane on a running turn pod: a phase chip + elapsed header, the per-round progress rail
/// (fed by CRUCIBLE_SCOPE_PROGRESS beats), and a collapsible auto-following log tail. Everything
/// here is a live viewport over `GET /api/turns/:pod/live` — the terminal report stays the scope
/// report card's job.
export function TurnLive({ podName, createdAt }: TurnLiveProps) {
  const { phase, beats, activity, logs, connState } = useTurnLive(podName);
  const [logsOpen, setLogsOpen] = useState(false);

  // Re-render each second while streaming so the elapsed readout ticks.
  const [, setTick] = useState(0);
  const streaming = connState.state !== 'ended';
  useLayoutEffect(() => {
    if (!streaming) return;
    const timer = window.setInterval(() => setTick((t) => t + 1), 1000);
    return () => window.clearInterval(timer);
  }, [streaming]);

  // Elapsed only ticks while the stream is open; after `end` the turns ledger owns the duration.
  const elapsed = streaming && createdAt ? turnDuration(createdAt, null) : null;
  const conn = connLabel(connState);

  return (
    <div className="border border-rule-hard bg-surface">
      <div className="flex flex-wrap items-center gap-3 border-b border-rule bg-sunk px-3.5 py-1.5">
        <Status
          status={phase ?? 'locating pod'}
          tone={phase ? statusTone(phaseColor(phase)) : 'grey'}
          pulse={phase === 'Running'}
        />
        {elapsed && <Mono tone="ink-3">running {elapsed}</Mono>}
        <span className="ml-auto">
          <Status status={conn.text} tone={conn.tone} pulse={conn.live} />
        </span>
      </div>

      {beats.length > 0 && (
        <div className="flex flex-col gap-1.5 border-b border-rule px-3.5 py-2">
          {beats.map((beat) => (
            <BeatRow key={`${beat.round}-${beat.kind}`} beat={beat} />
          ))}
        </div>
      )}

      {activity && streaming && <ActivityTicker beat={activity} />}

      <Button
        className="w-full justify-start"
        aria-expanded={logsOpen}
        onClick={() => {
          setLogsOpen(!logsOpen);
        }}
      >
        {logsOpen ? '▾ HIDE LIVE LOG' : `▸ LIVE LOG (${logs.length} LINES)`}
      </Button>
      {logsOpen && <LogPane logs={logs} />}
    </div>
  );
}

function BeatRow({ beat }: { beat: ScopeProgressBeat }) {
  return (
    <div className="flex items-baseline gap-2.5">
      <Mono size="micro" weight="bold" uppercase className="shrink-0 border border-rule-hard px-1">
        round {beat.round} · {beat.kind}
      </Mono>
      <Mono className="wrap-anywhere">{beat.doing}</Mono>
      <Mono tone="ink-3" className="ml-auto shrink-0">
        ${beat.cost_so_far.toFixed(4)} so far
      </Mono>
    </div>
  );
}

/// The within-round ticker: the newest activity beat (tool call / text snippet / usage / sandbox
/// stage) plus the running cost, so a long agent round is never a silent "round N · propose".
function ActivityTicker({ beat }: { beat: ScopeActivityBeat }) {
  return (
    <div className="flex items-baseline gap-2.5 border-b border-rule px-3.5 py-1.5">
      <Mono
        size="micro"
        weight="bold"
        uppercase
        tone={beat.kind === 'truncated' ? 'amber' : 'blue'}
        className="shrink-0 border border-rule-hard px-1"
      >
        {beat.kind}
      </Mono>
      <Mono className="truncate">{activityLabel(beat)}</Mono>
      <Mono tone="ink-3" className="ml-auto shrink-0">
        ${beat.cost_so_far.toFixed(4)} so far
      </Mono>
    </div>
  );
}

function LogPane({ logs }: { logs: LogLine[] }) {
  const { scrollRef, following, onScroll, resume } = useFollowScroll(logs);

  return (
    <div className="relative border-t border-rule">
      <div
        ref={scrollRef}
        onScroll={onScroll}
        className="max-h-80 overflow-y-auto bg-sunk px-3 py-2 font-mono text-data leading-[1.45]"
      >
        {logs.length === 0 ? (
          <Mono tone="ink-3" className="italic">
            Waiting for log output…
          </Mono>
        ) : (
          logs.map((line) => (
            <div key={line.id} className="whitespace-pre-wrap wrap-anywhere text-ink">
              {line.text}
            </div>
          ))
        )}
      </div>
      {!following && (
        <div className="absolute bottom-2 left-1/2 -translate-x-1/2">
          <Button variant="filled" onClick={resume}>
            ↓ RESUME FOLLOWING
          </Button>
        </div>
      )}
    </div>
  );
}
