import { useMemo } from 'react';
import type { ReactNode } from 'react';
import { cn, Status } from '../ui';
import { useLiveSession, type ConnState } from './useLiveSession';
import { connLabel } from './connLabel';
import { scorePoints } from './feed';
import { SessionFeed } from './SessionFeed';
import { Sparkline } from './Sparkline';
import type { LiveStatus } from './session';

interface LiveSessionProps {
  runId: string;
  /// Fired once when the stream ends, with the terminal reason. The parent uses this to refetch the
  /// run once its record has been ingested (on run-not-running / bridge-closed).
  onEnded?: (reason: string) => void;
  /// Wall-tile mode: shorter feed, sparkline, no filter toolbar.
  compact?: boolean;
}

/// The live pane on a running run: a status header fed by `status` snapshots, a live score
/// sparkline, and the shared session scrollback ([`SessionFeed`]) over the SSE-fed rows.
export function LiveSession({ runId, onEnded, compact = false }: LiveSessionProps) {
  const { status, connState, rows } = useLiveSession(runId, onEnded);
  const points = useMemo(() => scorePoints(rows), [rows]);

  return (
    <div className="overflow-hidden border border-rule-hard bg-surface">
      <StatusHeader status={status} connState={connState} />
      {points.length >= 2 && (
        <div className="border-b border-rule bg-sunk px-3.5 py-1.5">
          <Sparkline points={points} height={compact ? 28 : 40} />
        </div>
      )}
      <SessionFeed rows={rows} compact={compact} />
    </div>
  );
}

function Metric({ label, value, className }: { label: string; value: ReactNode; className?: string }) {
  return (
    <div className={cn('flex items-baseline gap-2 border-r border-rule px-3.5 py-[5px]', className)}>
      <dt className="uppercase tracking-group text-ink-3">{label}</dt>
      <dd className="m-0 text-data font-semibold text-ink">{value}</dd>
    </div>
  );
}

function StatusHeader({ status, connState }: { status: LiveStatus | null; connState: ConnState }) {
  const conn = connLabel(connState);
  const spend = status?.spend ?? 0;
  const cap = status?.max_cost;
  const pct = cap && cap > 0 ? Math.min(100, (spend / cap) * 100) : 0;
  const over = cap !== undefined && cap > 0 && spend > cap;

  return (
    <div className="flex flex-wrap items-stretch border-b border-rule bg-sunk font-mono text-label">
      <dl className="m-0 flex flex-wrap items-stretch">
        <Metric label="Phase" value={status?.phase ?? '—'} />
        <Metric label="Iteration" value={status ? status.iter : '—'} />
        <Metric
          label="Best score"
          value={status?.best_score !== undefined ? status.best_score.toFixed(1) : '—'}
        />
        <div className="min-w-40 border-r border-rule px-3.5 py-[5px]">
          <div className="flex items-baseline gap-2">
            <dt className="uppercase tracking-group text-ink-3">Spend</dt>
            <dd className="m-0 text-data font-semibold text-ink">
              ${spend.toFixed(2)}
              {cap !== undefined ? ` / $${cap.toFixed(2)}` : ''}
              {cap !== undefined && cap > 0 ? ` (${pct.toFixed(0)}%)` : ''}
            </dd>
          </div>
          {cap !== undefined && cap > 0 && (
            <div className="mt-1 h-1 bg-rule">
              <div className={cn('h-full', over ? 'bg-red' : 'bg-green')} style={{ width: `${pct}%` }} />
            </div>
          )}
        </div>
      </dl>
      <div className="ml-auto flex items-center gap-3 border-l border-rule px-3.5">
        {status?.paused && <Status status="paused" tone="amber" pulse={false} />}
        <Status status={conn.text} tone={conn.tone} pulse={conn.live} />
      </div>
    </div>
  );
}
