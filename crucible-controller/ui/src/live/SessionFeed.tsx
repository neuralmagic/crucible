import { useMemo, useState } from 'react';
import { Button, cn, Mono, Toolbar, ToolbarGroup, ToolbarSearch } from '../ui';
import {
  ALL_CATEGORIES,
  describeLine,
  rowCategory,
  rowMatches,
  type FeedCategory,
  type FeedRow,
  type LineTone,
} from './feed';
import { useFollowScroll } from './useFollowScroll';

const TONE_ACCENT: Record<LineTone, string> = {
  keep: 'border-l-green',
  drop: 'border-l-red',
  error: 'border-l-red',
  success: 'border-l-green',
  budget: 'border-l-amber',
  info: 'border-l-blue',
  muted: 'border-l-transparent',
};

interface SessionFeedProps {
  rows: FeedRow[];
  /// Wall-tile mode: shorter feed, no filter toolbar.
  compact?: boolean;
  /// Live mode sticks to the bottom as rows stream in; a post-facto replay starts at the top and
  /// never shows the resume-following affordance.
  autoFollow?: boolean;
  emptyText?: string;
}

/// The session scrollback shared by the live pane and the post-facto transcript replay: a
/// filter/search toolbar (full mode) over semantically-rendered session rows. Pure render over
/// already-folded rows — where they come from (SSE or a fetched session.jsonl) is the caller's
/// business.
export function SessionFeed({ rows, compact = false, autoFollow = true, emptyText = 'Waiting for live events…' }: SessionFeedProps) {
  const [categories, setCategories] = useState<FeedCategory[]>(ALL_CATEGORIES);
  const [query, setQuery] = useState('');

  const filtered = useMemo(() => {
    if (categories.length === ALL_CATEGORIES.length && query.trim() === '') return rows;
    return rows.filter((r) => categories.includes(rowCategory(r)) && rowMatches(r, query));
  }, [rows, categories, query]);

  const toggle = (cat: FeedCategory) => {
    setCategories((cur) => {
      const next = cur.includes(cat) ? cur.filter((c) => c !== cat) : [...cur, cat];
      // Never allow an empty set — an all-hidden feed reads as broken, not filtered.
      return next.length === 0 ? cur : next;
    });
  };

  return (
    <>
      {!compact && (
        <Toolbar>
          <ToolbarGroup label="Show">
            {ALL_CATEGORIES.map((cat) => (
              <Button
                key={cat}
                selected={categories.includes(cat)}
                onClick={() => {
                  toggle(cat);
                }}
              >
                {cat}
              </Button>
            ))}
          </ToolbarGroup>
          <ToolbarSearch
            value={query}
            onChange={setQuery}
            placeholder="Filter events…"
            aria-label="Search the session feed"
          />
        </Toolbar>
      )}
      <SessionScrollback
        rows={filtered}
        hidden={rows.length - filtered.length}
        compact={compact}
        autoFollow={autoFollow}
        emptyText={emptyText}
      />
    </>
  );
}

function SessionScrollback({ rows, hidden, compact, autoFollow, emptyText }: { rows: FeedRow[]; hidden: number; compact: boolean; autoFollow: boolean; emptyText: string }) {
  const { scrollRef, following, onScroll, resume } = useFollowScroll(rows, autoFollow);

  return (
    <div className="relative">
      <div
        ref={scrollRef}
        onScroll={onScroll}
        className={cn(
          'overflow-y-auto bg-sunk py-1 font-mono text-data leading-[1.5]',
          compact ? 'max-h-56' : 'max-h-[26rem]',
        )}
      >
        {rows.length === 0 ? (
          <div className="px-3.5 py-3.5">
            <Mono tone="ink-3">
              {hidden > 0 ? `All ${hidden} events hidden by the current filter` : emptyText}
            </Mono>
          </div>
        ) : (
          rows.map((row) => <FeedRowView key={row.id} row={row} />)
        )}
        {rows.length > 0 && hidden > 0 && (
          <div className="px-3.5 py-0.5">
            <Mono size="micro" tone="ink-3">
              {hidden} events hidden by filters
            </Mono>
          </div>
        )}
      </div>
      {autoFollow && !following && (
        <div className="absolute bottom-3 left-1/2 -translate-x-1/2">
          <Button variant="filled" onClick={resume}>
            ↓ RESUME FOLLOWING
          </Button>
        </div>
      )}
    </div>
  );
}

function FeedRowView({ row }: { row: FeedRow }) {
  if (row.kind === 'text') {
    return <div className="px-3.5 py-0.5 whitespace-pre-wrap wrap-anywhere text-ink">{row.text}</div>;
  }
  if (row.kind === 'thinking') {
    return (
      <div className="px-3.5 py-0.5 whitespace-pre-wrap wrap-anywhere text-ink-3 italic">{row.text}</div>
    );
  }
  if (row.kind === 'pod') {
    return (
      <div className="flex items-baseline gap-2.5 border-l-2 border-l-transparent px-3.5 py-0.5">
        <span className="w-24 shrink-0 text-label uppercase tracking-label text-ink-3">pod</span>
        <div className="min-w-0 flex-1 whitespace-pre-wrap wrap-anywhere font-mono text-ink-2">
          {row.text}
        </div>
      </div>
    );
  }
  const view = describeLine(row.line);
  return (
    <div className={cn('flex items-baseline gap-2.5 border-l-2 px-3.5 py-0.5', TONE_ACCENT[view.tone])}>
      <span className="w-24 shrink-0 text-label uppercase tracking-label text-ink-3">{view.tag}</span>
      <div className="min-w-0 flex-1">
        <div className="whitespace-pre-wrap wrap-anywhere text-ink">{view.title}</div>
        {view.detail && (
          <div className="whitespace-pre-wrap wrap-anywhere text-ink-3">{view.detail}</div>
        )}
      </div>
      {view.score !== undefined && (
        <span className="shrink-0 font-semibold text-ink">{view.score.toFixed(1)}</span>
      )}
    </div>
  );
}
