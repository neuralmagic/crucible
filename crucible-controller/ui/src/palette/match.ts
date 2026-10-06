const words = (text: string) =>
  text
    .toLowerCase()
    .split(/[^a-z0-9]+/)
    .filter(Boolean);

/**
 * How well `search` matches an item, in [0, 1]; 0 hides it. Every search term must appear in the
 * title or the other fields. A term counts most at the start of the title, then at the start of a
 * title word, then anywhere in the title, then at the start of a word elsewhere, then anywhere.
 */
export const score = (search: string, title: string, rest: string[]): number => {
  const terms = words(search);
  if (terms.length === 0) return 1;
  const lowerTitle = title.toLowerCase();
  const titleWords = words(title);
  const restText = rest.join(' ').toLowerCase();
  const restWords = words(restText);
  let total = 0;
  for (const term of terms) {
    let best = 0;
    if (lowerTitle.startsWith(term)) best = 1;
    else if (titleWords.some((w) => w.startsWith(term))) best = 0.8;
    else if (lowerTitle.includes(term)) best = 0.6;
    else if (restWords.some((w) => w.startsWith(term))) best = 0.4;
    else if (restText.includes(term)) best = 0.2;
    if (best === 0) return 0;
    total += best;
  }
  return total / terms.length;
};
