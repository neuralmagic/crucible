import type { ReactNode } from 'react';
import { cn } from './cn';

export type MonoSize = 'micro' | 'label' | 'data' | 'data-lg';
export type MonoTone = 'ink' | 'ink-2' | 'ink-3' | 'green' | 'amber' | 'red' | 'blue';
export type MonoWeight = 'normal' | 'medium' | 'semibold' | 'bold';

const SIZE: Record<MonoSize, string> = {
  micro: 'text-micro',
  label: 'text-label',
  data: 'text-data',
  'data-lg': 'text-data-lg',
};

const TONE: Record<MonoTone, string> = {
  ink: 'text-ink',
  'ink-2': 'text-ink-2',
  'ink-3': 'text-ink-3',
  green: 'text-green',
  amber: 'text-amber',
  red: 'text-red',
  blue: 'text-blue',
};

const WEIGHT: Record<MonoWeight, string> = {
  normal: 'font-normal',
  medium: 'font-medium',
  semibold: 'font-semibold',
  bold: 'font-bold',
};

export interface MonoProps {
  children: ReactNode;
  size?: MonoSize;
  tone?: MonoTone;
  weight?: MonoWeight;
  uppercase?: boolean;
  title?: string;
  className?: string;
}

export function Mono({
  children,
  size = 'data',
  tone = 'ink-2',
  weight = 'normal',
  uppercase = false,
  title,
  className,
}: MonoProps) {
  return (
    <span
      title={title}
      className={cn(
        'font-mono',
        SIZE[size],
        TONE[tone],
        WEIGHT[weight],
        uppercase && 'uppercase tracking-label',
        className,
      )}
    >
      {children}
    </span>
  );
}
