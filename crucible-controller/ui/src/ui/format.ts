/** An RFC3339 stamp in the viewer's locale; an em-dash when absent, the raw string when unparseable. */
export function formatStamp(iso: string | null | undefined): string {
  if (!iso) return '—';
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
}
