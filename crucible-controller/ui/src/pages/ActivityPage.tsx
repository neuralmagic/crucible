import { useEffect, useState } from 'react';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import type { components } from '../api/schema.d';
import { openEventSource } from '../api/session';
import { issueStatusColor } from './issueStatus';
import {
  createDataColumnHelper,
  DataTable,
  Empty,
  Identifier,
  LoadingBlock,
  Mono,
  PageHeader,
  Section,
  SectionHeader,
  Status,
  statusTone,
  useDataTable,
} from '../ui';
import { detailPath } from './launchView';
import { Stamp } from './Stamp';

type EventDto = components['schemas']['EventDto'];
type LongText = components['schemas']['LongText'];

const MAX_EVENTS = 50;

/** Pull a `reason` out of an untyped SSE frame. The stream serves the truncated shape, so the
 * flag is read off the wire rather than assumed. */
function parseLongText(v: object): LongText | null {
  if (!('reason' in v) || typeof v.reason !== 'object' || v.reason === null) return null;
  const reason: object = v.reason;
  if (!('text' in reason) || typeof reason.text !== 'string') return null;
  return { text: reason.text, truncated: 'truncated' in reason && reason.truncated === true };
}

function parseEvent(raw: string): EventDto | null {
  let v: unknown;
  try {
    v = JSON.parse(raw);
  } catch {
    return null;
  }
  if (
    typeof v === 'object' &&
    v !== null &&
    'ts' in v &&
    typeof v.ts === 'string' &&
    'key' in v &&
    typeof v.key === 'string' &&
    'from' in v &&
    typeof v.from === 'string' &&
    'to' in v &&
    typeof v.to === 'string'
  ) {
    return {
      ts: v.ts,
      key: v.key,
      from: v.from,
      to: v.to,
      reason: parseLongText(v),
      actor: 'actor' in v && typeof v.actor === 'string' ? v.actor : null,
      evidence: 'evidence' in v && typeof v.evidence === 'string' ? v.evidence : null,
    };
  }
  return null;
}

function getDayHeader(isoTs: string): string {
  const date = new Date(isoTs);
  const today = new Date();
  const yesterday = new Date(today);
  yesterday.setDate(today.getDate() - 1);

  const isToday = date.toDateString() === today.toDateString();
  const isYesterday = date.toDateString() === yesterday.toDateString();

  if (isToday) return 'Today';
  if (isYesterday) return 'Yesterday';
  return date.toLocaleDateString(undefined, { month: 'short', day: 'numeric', year: 'numeric' });
}

function groupEventsByDay(events: EventDto[]): Map<string, EventDto[]> {
  const groups = new Map<string, EventDto[]>();
  for (const event of events) {
    const dayKey = getDayHeader(event.ts);
    const existing = groups.get(dayKey);
    if (existing) {
      existing.push(event);
    } else {
      groups.set(dayKey, [event]);
    }
  }
  return groups;
}

const helper = createDataColumnHelper<EventDto>();

const columns = helper.columns([
  helper.accessor('ts', {
    header: 'When',
    enableSorting: false,
    meta: { shrink: true },
    cell: ({ getValue }) => <Stamp iso={getValue()} />,
  }),
  helper.accessor('key', {
    header: 'Issue',
    enableSorting: false,
    meta: { pad: 'tight', shrink: true },
    cell: ({ getValue }) => (
      <Identifier to={detailPath(getValue())} title={getValue()}>
        {getValue()}
      </Identifier>
    ),
  }),
  helper.display({
    id: 'transition',
    header: 'Transition',
    meta: { shrink: true },
    cell: ({ row }) => (
      <span className="flex items-center gap-2">
        <Status
          status={row.original.from}
          tone={statusTone(issueStatusColor(row.original.from))}
        />
        <Mono tone="ink-3">→</Mono>
        <Status status={row.original.to} tone={statusTone(issueStatusColor(row.original.to))} />
      </span>
    ),
  }),
  helper.accessor('actor', {
    header: 'Actor',
    enableSorting: false,
    meta: { shrink: true },
    cell: ({ getValue }) => {
      const actor = getValue();
      return actor && actor.trim() !== '' ? <Mono>{actor}</Mono> : <Mono tone="ink-3">—</Mono>;
    },
  }),
  helper.display({
    id: 'reason',
    header: 'Reason',
    meta: { wrap: true, width: '48%' },
    cell: ({ row }) => <span className="text-ink-2">{row.original.reason?.text || '—'}</span>,
  }),
]);

function DayTable({ day, events }: { day: string; events: EventDto[] }) {
  const table = useDataTable({
    columns,
    data: events,
    getRowId: (event, index) => `${event.ts}-${event.key}-${String(index)}`,
  });
  return (
    <Section>
      <SectionHeader title={day} note={`${events.length} transition${events.length === 1 ? '' : 's'}`} />
      <DataTable table={table} />
    </Section>
  );
}

export function ActivityPage() {
  const initialEvents = $api.useQuery('get', '/api/events');
  const [liveEvents, setLiveEvents] = useState<EventDto[]>([]);
  const [eventSourceReady, setEventSourceReady] = useState(false);

  useEffect(() => {
    const eventSource = openEventSource('/api/events/stream');

    eventSource.onopen = () => {
      setEventSourceReady(true);
    };

    eventSource.onmessage = (msg: MessageEvent<string>) => {
      const event = parseEvent(msg.data);
      if (event === null) return;
      setLiveEvents((prev) => [event, ...prev].slice(0, MAX_EVENTS));
    };

    eventSource.onerror = () => {
      setEventSourceReady(false);
    };

    return () => {
      eventSource.close();
    };
  }, []);

  const allEvents = [...liveEvents, ...(initialEvents.data ?? [])].slice(0, MAX_EVENTS);
  const eventsByDay = groupEventsByDay(allEvents);

  return (
    <>
      <PageHeader
        eyebrow="Records"
        title="Activity"
        description="Every status transition the controller recorded, newest first."
        actions={
          <Status
            status={eventSourceReady ? 'live' : 'disconnected'}
            tone={eventSourceReady ? 'green' : 'red'}
            pulse={eventSourceReady}
          />
        }
      />

      {initialEvents.isError ? (
        <Empty title="ACTIVITY UNAVAILABLE" description={formatError(initialEvents.error)} />
      ) : initialEvents.isPending ? (
        <LoadingBlock label="LOADING ACTIVITY" />
      ) : allEvents.length === 0 ? (
        <Empty title="NO TRANSITIONS" description="Nothing has moved through the loop yet." />
      ) : (
        Array.from(eventsByDay.entries()).map(([day, events]) => (
          <DayTable key={day} day={day} events={events} />
        ))
      )}
    </>
  );
}
