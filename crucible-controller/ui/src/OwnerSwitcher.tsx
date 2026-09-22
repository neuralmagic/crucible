import { Popover } from '@base-ui-components/react/popover';
import { ALL, type ActedAs } from './ownerContext';
import { useOwnerContext } from './useOwnerContext';
import { cn } from './ui';

function Choice({ value, label, note, active, onPick }: { value: string; label: string; note?: string; active: boolean; onPick: (value: string) => void }) {
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

function current(context: string, principals: readonly ActedAs[]): string {
  if (context === ALL) return 'ALL';
  return principals.find((p) => p.value === context)?.label ?? context;
}

/// The owner context in the masthead: everything the caller may read, or one principal's
/// resources. Every list and creation form follows it.
export function OwnerSwitcher() {
  const owner = useOwnerContext();
  if (!owner.ready || owner.switchable.length === 0) return null;

  return (
    <Popover.Root>
      <Popover.Trigger
        data-testid="owner-switcher"
        className="flex items-center gap-[7px] border-l border-rule px-3 font-mono text-data text-ink hover:bg-hi"
      >
        <span className="text-ink-3">AS</span>
        <span className="font-medium">{current(owner.context, owner.switchable)}</span>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Positioner sideOffset={1} align="end">
          <Popover.Popup data-testid="owner-switcher-menu" className="min-w-56 border border-rule-hard bg-surface">
            <Choice value={ALL} label="ALL" active={owner.context === ALL} onPick={owner.setContext} />
            {owner.switchable.map((p) => (
              <Choice
                key={p.value}
                value={p.value}
                label={p.value}
                note={p.kind === 'team' ? p.role : undefined}
                active={owner.context === p.value}
                onPick={owner.setContext}
              />
            ))}
          </Popover.Popup>
        </Popover.Positioner>
      </Popover.Portal>
    </Popover.Root>
  );
}
