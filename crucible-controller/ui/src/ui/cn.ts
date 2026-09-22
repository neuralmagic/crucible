import { extendTailwindMerge } from 'tailwind-merge';

type ClassValue = string | false | null | undefined;

const merge = extendTailwindMerge({
  extend: {
    theme: {
      text: [
        'micro',
        'label',
        'data',
        'data-lg',
        'body',
        'wordmark',
        'lede',
        'figure',
        'title',
        'display',
      ],
      tracking: ['title', 'display', 'data', 'action', 'label', 'group', 'section', 'eyebrow', 'brand'],
      color: [
        'paper',
        'surface',
        'sunk',
        'ink',
        'ink-2',
        'ink-3',
        'rule',
        'rule-hard',
        'green',
        'amber',
        'red',
        'blue',
        'hi',
      ],
      spacing: ['row'],
    },
  },
});

/** Join utility classes, last conflicting utility wins, so a caller's `className` always
 * overrides the component's own. */
export function cn(...parts: readonly ClassValue[]): string {
  return merge(...parts);
}
