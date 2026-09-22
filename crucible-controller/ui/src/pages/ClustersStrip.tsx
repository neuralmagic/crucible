import type { components } from '../api/schema.d';
import { $api } from '../api/client';
import { cn, Mono, Section, SectionHeader, Status } from '../ui';

type ClusterSnapshot = components['schemas']['ClusterSnapshot'];
type GpuPool = components['schemas']['GpuPool'];
type KueueQueue = components['schemas']['KueueQueue'];

/** "NVIDIA-A100-SXM4-80GB" reads fine without the vendor prefix; anything else verbatim. */
function poolLabel(pool: string): string {
  return pool.startsWith('NVIDIA-') ? pool.slice('NVIDIA-'.length) : pool;
}

function formatAge(secs: number): string {
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  return `${Math.floor(secs / 3600)}h ago`;
}

function fillClass(ratio: number, reachable: boolean): string {
  if (!reachable) return 'bg-ink-3';
  if (ratio >= 0.85) return 'bg-red';
  if (ratio >= 0.6) return 'bg-amber';
  return 'bg-green';
}

function PoolBar({ pool, reachable }: { pool: GpuPool; reachable: boolean }) {
  const ratio = pool.allocatable === 0 ? 0 : pool.requested / pool.allocatable;
  return (
    <div className="grid grid-cols-[9rem_minmax(0,1fr)_3.2rem] items-center gap-2">
      <Mono size="label" tone="ink-3" title={pool.pool} className="truncate">
        {poolLabel(pool.pool)}
      </Mono>
      <div className="h-1.5 bg-sunk">
        <div
          className={cn('h-full', fillClass(ratio, reachable))}
          style={{ width: `${Math.min(100, ratio * 100)}%` }}
        />
      </div>
      <Mono size="label" className="text-right">
        {pool.requested} / {pool.allocatable}
      </Mono>
    </div>
  );
}

function KueueChips({ queues }: { queues: KueueQueue[] | null | undefined }) {
  if (queues == null || queues.length === 0) {
    return (
      <Mono size="label" tone="ink-3" uppercase>
        {queues == null ? 'no kueue on this cluster' : 'no clusterqueues defined'}
      </Mono>
    );
  }
  return (
    <div className="flex flex-wrap gap-1.5">
      {queues.map((q) => (
        <span
          key={q.queue}
          className="flex items-baseline gap-1.5 border border-rule px-1.5 py-0.5 font-mono text-label text-ink-2"
        >
          {q.queue}
          <span className="font-semibold text-ink">
            {q.gpu_reserved}/{q.gpu_nominal}
          </span>
          {q.pending > 0 ? <span className="font-semibold text-amber">{q.pending} pending</span> : null}
        </span>
      ))}
    </div>
  );
}

function ClusterPanel({ snap }: { snap: ClusterSnapshot }) {
  return (
    <div className={cn('flex flex-col gap-2 px-4.5 py-3.5', !snap.reachable && 'opacity-60')}>
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <Mono size="data-lg" tone="ink" weight="semibold">
          {snap.cluster}
        </Mono>
        <Status
          status={snap.reachable ? 'reachable' : 'unreachable'}
          tone={snap.reachable ? 'green' : 'red'}
        />
      </div>
      <Mono size="label" tone="ink-3">
        snapshot {formatAge(snap.age_secs)}
      </Mono>
      {snap.pools.length === 0 ? (
        <Mono size="label" tone="ink-3" uppercase>
          no gpu pools
        </Mono>
      ) : (
        snap.pools.map((pool) => <PoolBar key={pool.pool} pool={pool} reachable={snap.reachable} />)
      )}
      <KueueChips queues={snap.kueue} />
      {snap.error ? (
        <Mono size="label" tone="red" title={snap.error} className="truncate">
          {snap.error}
        </Mono>
      ) : null}
    </div>
  );
}

/** The connected-clusters row: is each dispatch target busy, before you dispatch. */
export function ClustersStrip() {
  const clusters = $api.useQuery('get', '/api/clusters', {}, { refetchInterval: 30_000 });
  if (!clusters.isSuccess || clusters.data.length === 0) return null;
  return (
    <Section>
      <SectionHeader title="Connected clusters" note="gpu pools and kueue pressure" />
      <div className="grid grid-cols-1 bg-surface wide:grid-cols-3 wide:divide-x wide:divide-rule">
        {clusters.data.map((snap) => (
          <ClusterPanel key={snap.cluster} snap={snap} />
        ))}
      </div>
    </Section>
  );
}
