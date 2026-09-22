import type { ReactNode } from 'react';
import { Group, Panel, Separator, useDefaultLayout } from 'react-resizable-panels';
import { cn } from './cn';
import { deviceStorage } from '../deviceStore';

export interface SplitProps {
  /// Unique per surface: the layout is remembered under it in this browser.
  id: string;
  /// The panels this group renders, in order. Conditional panels change the key a layout is
  /// remembered under, so a two-pane arrangement never restores into a one-pane one.
  panelIds: readonly string[];
  orientation?: 'horizontal' | 'vertical';
  className?: string;
  children: ReactNode;
}

/// A group of resizable panes. Dragging a divider resizes, double-clicking it restores the
/// defaults, and where the divider was left is remembered per device.
///
/// The library stamps inline `height: 100%` on the group, which beats any height utility in
/// `className` and collapses to nothing inside an auto-height parent — so the classes go on a
/// wrapper the percentage can resolve against.
export function Split({ id, panelIds, orientation = 'horizontal', className, children }: SplitProps) {
  const { defaultLayout, onLayoutChanged } = useDefaultLayout({
    id,
    panelIds: [...panelIds],
    storage: deviceStorage,
  });

  return (
    <div className={className}>
      <Group
        id={id}
        orientation={orientation}
        defaultLayout={defaultLayout}
        onLayoutChanged={onLayoutChanged}
      >
        {children}
      </Group>
    </div>
  );
}

export interface SplitPaneProps {
  id: string;
  defaultSize?: number | string;
  minSize?: number | string;
  maxSize?: number | string;
  className?: string;
  children: ReactNode;
}

export function SplitPane({ id, defaultSize, minSize, maxSize, className, children }: SplitPaneProps) {
  return (
    <Panel
      id={id}
      defaultSize={defaultSize}
      minSize={minSize}
      maxSize={maxSize}
      className={cn('flex min-h-0 min-w-0 flex-col', className)}
    >
      {children}
    </Panel>
  );
}

export interface SplitHandleProps {
  label: string;
  orientation?: 'horizontal' | 'vertical';
}

/// The divider itself. The library gives it the keyboard behaviour of a separator; the hit target
/// is wider than the rule it draws so it can be grabbed at all.
export function SplitHandle({ label, orientation = 'horizontal' }: SplitHandleProps) {
  const across = orientation === 'horizontal';
  return (
    <Separator
      aria-label={label}
      className={cn(
        'group flex bg-transparent focus-visible:outline-none',
        across ? 'w-[7px] items-stretch justify-center' : 'h-[7px] items-center justify-stretch'
      )}
    >
      <span
        aria-hidden
        className={cn(
          'bg-rule-hard group-hover:bg-ink group-focus-visible:bg-blue',
          across ? 'w-px' : 'h-px w-full'
        )}
      />
    </Separator>
  );
}
