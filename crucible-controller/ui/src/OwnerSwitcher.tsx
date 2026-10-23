import { Popover } from '@base-ui-components/react/popover';
import { useSyncExternalStore, type ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { actAsFor, actAsRefusal } from './actAs';
import { subscribeFlags } from './deviceStore';
import { ALL, type ActedAs } from './ownerContext';
import { useOwnerContext } from './useOwnerContext';
import { pickTeams } from './teamPicks';
import { useActiveTeams, useStarredTeams } from './useTeamPicks';
import { cn } from './ui';

function Choice({ value, label, note, active, onPick }: { value: string; label: ReactNode; note?: string; active: boolean; onPick: (value: string) => void }) {
  return (
    <Popover.Close
      render={<button type="button" />}
      aria-pressed={active}
      data-value={value}
      onClick={() => {
        onPick(value);
      }}
      className={cn(
        'flex w-full cursor-pointer items-center justify-between gap-6 border-0 border-b border-rule bg-transparent px-3 py-2 text-left font-mono text-data last:border-b-0 hover:bg-hi',
        active ? 'font-semibold text-ink' : 'text-ink-2',
      )}
    >
      <span>{label}</span>
      {note === undefined ? null : <span className="text-ink-3">{note}</span>}
    </Popover.Close>
  );
}

function current(context: string, principals: readonly ActedAs[]): ReactNode {
  if (context === ALL) return <span className="uppercase">All</span>;
  return principals.find((p) => p.value === context)?.label ?? context;
}

/// The owner context in the masthead: everything the caller may read, or one principal's
/// resources. Every list and creation form follows it.
export function OwnerSwitcher() {
  const owner = useOwnerContext();
  const refused = useSyncExternalStore(subscribeFlags, actAsRefusal, () => null);
  const [starred] = useStarredTeams();
  const active = useActiveTeams();
  if (!owner.ready || owner.switchable.length === 0) return null;
  const team = actAsFor(owner.stored);
  const picked = pickTeams(
    owner.switchable.filter((p) => p.kind === 'team').map((p) => p.label),
    starred,
    active,
    team === null ? null : team.slice('team:'.length),
  );
  const named = new Set(picked.shown.map((pick) => `team:${pick.slug}`));
  const choices = [
    ...owner.switchable.filter((p) => p.kind !== 'team'),
    ...picked.shown.flatMap((pick) => owner.switchable.filter((p) => p.value === `team:${pick.slug}`)),
  ];

  return (
    <>
      {refused === null ? null : (
        <span
          role="status"
          data-testid="act-as-refused"
          className="flex max-w-80 items-center truncate border-l border-rule px-3 font-mono text-data text-red"
          title={refused}
        >
          {refused}
        </span>
      )}
      <Popover.Root>
        <Popover.Trigger
          data-testid="owner-switcher"
          data-acting-as={team ?? undefined}
          title={team === null ? undefined : `Acting as ${team}: every page answers for this team`}
          className={cn(
            'flex items-center gap-[7px] border-l border-rule px-3 font-mono text-data hover:bg-hi',
            team === null ? 'text-ink' : 'bg-sunk text-amber',
          )}
        >
          <span className={cn('uppercase', team === null && 'text-ink-3')}>{team === null ? 'As' : 'Acting as'}</span>
          <span className="font-medium">{current(owner.stored, owner.switchable)}</span>
        </Popover.Trigger>
        <Popover.Portal>
          <Popover.Positioner sideOffset={1} align="end">
            <Popover.Popup data-testid="owner-switcher-menu" className="min-w-56 border border-rule-hard bg-surface">
              <Choice value={ALL} label={<span className="uppercase">All</span>} active={owner.stored === ALL} onPick={owner.setContext} />
              {choices.filter((p) => p.kind !== 'team' || named.has(p.value)).map((p) => (
                <Choice
                  key={p.value}
                  value={p.value}
                  label={p.value}
                  note={p.kind === 'team' ? p.role : undefined}
                  active={owner.stored === p.value}
                  onPick={owner.setContext}
                />
              ))}
              {picked.hidden > 0 ? (
                <Popover.Close
                  render={<Link to="/teams" />}
                  nativeButton={false}
                  data-testid="owner-switcher-more"
                  className="flex w-full items-center justify-between gap-6 px-3 py-2 font-mono text-data text-ink-3 hover:bg-hi hover:text-ink"
                >
                  <span>All teams</span>
                  <span>{picked.hidden} more</span>
                </Popover.Close>
              ) : null}
            </Popover.Popup>
          </Popover.Positioner>
        </Popover.Portal>
      </Popover.Root>
    </>
  );
}
