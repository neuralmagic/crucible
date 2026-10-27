/** An RFC3339 stamp in the viewer's locale; an em-dash when absent, the raw string when unparseable. */
export function formatStamp(iso: string | null | undefined): string {
  if (!iso) return '—';
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
}

/** A value cut to its first `head` and last `tail` characters, joined by an ellipsis, when longer. */
export function middleTruncate(value: string, head: number, tail: number): string {
  if (value.length <= head + tail + 1) return value;
  return `${value.slice(0, head)}\u2026${value.slice(value.length - tail)}`;
}
