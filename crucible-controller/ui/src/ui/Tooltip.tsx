import { Tooltip as BaseTooltip } from '@base-ui-components/react/tooltip';
import type { ReactNode } from 'react';
import { cn } from './cn';

export type TooltipSide = 'top' | 'right' | 'bottom' | 'left';

export interface TooltipProps {
  /** Tooltip body. Nothing renders when this is undefined. */
  content: ReactNode;
  children: ReactNode;
  side?: TooltipSide;
  delay?: number;
  className?: string;
}

export function Tooltip({ content, children, side = 'top', delay = 400, className }: TooltipProps) {
  if (content === undefined || content === null || content === false) {
    return <>{children}</>;
  }
  return (
    <BaseTooltip.Root>
      <BaseTooltip.Trigger delay={delay} render={<span className="inline-flex" />}>
        {children}
      </BaseTooltip.Trigger>
      <BaseTooltip.Portal>
        <BaseTooltip.Positioner side={side} sideOffset={6}>
          <BaseTooltip.Popup
            className={cn(
              'max-w-[52ch] border border-rule-hard bg-surface px-2 py-1 font-mono text-data text-ink',
              className,
            )}
          >
            {content}
          </BaseTooltip.Popup>
        </BaseTooltip.Positioner>
      </BaseTooltip.Portal>
    </BaseTooltip.Root>
  );
}
