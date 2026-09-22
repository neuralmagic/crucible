import type { ReactNode } from 'react';
import { Button } from './Button';
import { cn } from './cn';

export interface ToolbarProps {
  children: ReactNode;
  className?: string;
}

export function Toolbar({ children, className }: ToolbarProps) {
  return (
    <div className={cn('flex items-stretch border-b border-rule-hard bg-surface', className)}>
      {children}
    </div>
  );
}

export interface ToolbarOption<T extends string> {
  value: T;
  label: string;
}

export type ToolbarGroupProps<T extends string> = {
  label: string;
  className?: string;
} & (
  | { children: ReactNode; options?: never; value?: never; onChange?: never }
  | {
      options: readonly ToolbarOption<T>[];
      value: T;
      onChange: (value: T) => void;
      children?: never;
    }
);

export function ToolbarGroup<T extends string>(props: ToolbarGroupProps<T>) {
  const { label, className } = props;
  return (
    <div
      className={cn(
        'flex border-r border-rule [&>*]:border-r [&>*]:border-rule [&>*:last-child]:border-r-0',
        className,
      )}
    >
      <span className="flex items-center border-r border-rule px-2.5 font-mono text-label uppercase tracking-group text-ink-3">
        {label}
      </span>
      {props.options === undefined
        ? props.children
        : props.options.map((option) => (
            <Button
              key={option.value}
              selected={option.value === props.value}
              onClick={() => {
                props.onChange(option.value);
              }}
            >
              {option.label}
            </Button>
          ))}
    </div>
  );
}

export interface ToolbarSearchProps {
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  'aria-label'?: string;
  className?: string;
}

export function ToolbarSearch({
  value,
  onChange,
  placeholder,
  'aria-label': ariaLabel,
  className,
}: ToolbarSearchProps) {
  return (
    <div className={cn('flex flex-1 items-center px-2.5', className)}>
      <input
        type="search"
        value={value}
        aria-label={ariaLabel ?? placeholder}
        placeholder={placeholder}
        onChange={(event) => {
          onChange(event.target.value);
        }}
        className="w-full border-0 bg-transparent py-1.5 font-mono text-data-lg text-ink placeholder:text-ink-3"
      />
    </div>
  );
}

export interface ToolbarActionsProps {
  children: ReactNode;
  className?: string;
}

export function ToolbarActions({ children, className }: ToolbarActionsProps) {
  return (
    <div className={cn('flex border-l border-rule [&>*]:h-full', className)}>{children}</div>
  );
}
