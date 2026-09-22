import { cloneElement } from 'react';
import type { ButtonHTMLAttributes, ReactElement, ReactNode } from 'react';
import { cn } from './cn';

export type ButtonVariant = 'filled' | 'quiet';

const BASE =
  'inline-flex items-center justify-center font-mono text-data tracking-action cursor-pointer disabled:cursor-default disabled:text-ink-3';

const VARIANT: Record<ButtonVariant, string> = {
  filled: 'bg-ink px-3.5 py-1.5 font-semibold text-surface',
  quiet: 'border-0 bg-transparent px-2.5 py-1.5 text-ink-2 hover:bg-hi hover:text-ink',
};

const SELECTED: Record<ButtonVariant, string> = {
  filled: '',
  quiet: 'bg-ink font-semibold text-surface hover:bg-ink hover:text-surface',
};

interface RenderProps {
  className?: string;
  children?: ReactNode;
}

export interface ButtonProps extends Omit<ButtonHTMLAttributes<HTMLButtonElement>, 'className'> {
  children: ReactNode;
  variant?: ButtonVariant;
  /** Toggle state for filter buttons: paints the quiet button as the active one. */
  selected?: boolean;
  /** Element to paint with the button styling instead of a `<button>`, e.g. `<Link to="…" />`. */
  render?: ReactElement<RenderProps>;
  className?: string;
}

export function Button({
  children,
  variant = 'quiet',
  selected = false,
  type = 'button',
  render,
  className,
  ...rest
}: ButtonProps) {
  const cls = cn(BASE, VARIANT[variant], selected && SELECTED[variant], className);
  if (render !== undefined) {
    return cloneElement(render, {
      ...rest,
      className: cn(cls, render.props.className),
      children,
    });
  }
  return (
    <button
      {...rest}
      type={type}
      aria-pressed={selected ? true : undefined}
      className={cls}
    >
      {children}
    </button>
  );
}
