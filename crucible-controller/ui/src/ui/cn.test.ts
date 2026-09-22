import { describe, expect, it } from 'vitest';
import { cn } from './cn';

describe('cn', () => {
  it('drops falsy parts', () => {
    expect(cn('inline-flex', false, null, undefined, 'items-center')).toBe(
      'inline-flex items-center',
    );
  });

  it('lets a later font-size token override an earlier one', () => {
    expect(cn('font-mono text-data', 'text-title')).toBe('font-mono text-title');
    expect(cn('text-data-lg', 'text-figure')).toBe('text-figure');
    expect(cn('text-micro', 'text-[19px]')).toBe('text-[19px]');
  });

  it('lets a later tracking token override an earlier one', () => {
    expect(cn('uppercase tracking-label', 'tracking-eyebrow')).toBe('uppercase tracking-eyebrow');
    expect(cn('tracking-label', 'tracking-[0.2em]')).toBe('tracking-[0.2em]');
  });

  it('lets a later text color override an earlier one', () => {
    expect(cn('text-ink-2', 'text-green')).toBe('text-green');
  });

  it('keeps a font size and a text color side by side', () => {
    expect(cn('text-label', 'text-ink')).toBe('text-label text-ink');
  });

  it('resolves padding, border color, and row-height conflicts', () => {
    expect(cn('px-4.5 py-3.5', 'px-0')).toBe('py-3.5 px-0');
    expect(cn('border-b border-rule', 'border-rule-hard')).toBe('border-b border-rule-hard');
    expect(cn('h-row', 'h-auto')).toBe('h-auto');
  });
});
