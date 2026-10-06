/// How an evidence file is shown.
export type FileView = 'image' | 'sandboxed' | 'markdown' | 'csv' | 'json' | 'text' | 'download';

export function fileView(mediaType: string): FileView {
  switch (mediaType) {
    case 'image/png':
    case 'image/jpeg':
    case 'image/gif':
      return 'image';
    case 'image/svg+xml':
    case 'text/html':
      return 'sandboxed';
    case 'text/markdown':
      return 'markdown';
    case 'text/csv':
      return 'csv';
    case 'application/json':
      return 'json';
    case 'text/plain':
      return 'text';
    default:
      return 'download';
  }
}

/// The file's bytes as UTF-8 text.
export function decodeText(base64: string): string {
  const binary = atob(base64);
  const bytes = Uint8Array.from(binary, (c) => c.charCodeAt(0));
  return new TextDecoder().decode(bytes);
}

/// Rows of an RFC 4180 CSV: quoted fields may hold commas, newlines, and doubled quotes.
export function parseCsv(text: string): string[][] {
  const rows: string[][] = [];
  let row: string[] = [];
  let field = '';
  let quoted = false;
  for (let i = 0; i < text.length; i += 1) {
    const c = text.charAt(i);
    if (quoted) {
      if (c === '"' && text.charAt(i + 1) === '"') {
        field += '"';
        i += 1;
      } else if (c === '"') {
        quoted = false;
      } else {
        field += c;
      }
    } else if (c === '"') {
      quoted = true;
    } else if (c === ',') {
      row.push(field);
      field = '';
    } else if (c === '\n' || c === '\r') {
      if (c === '\r' && text.charAt(i + 1) === '\n') i += 1;
      row.push(field);
      rows.push(row);
      row = [];
      field = '';
    } else {
      field += c;
    }
  }
  if (field !== '' || row.length > 0) {
    row.push(field);
    rows.push(row);
  }
  return rows;
}

/// The policy an untrusted evidence document runs under: inline script and style only, no network,
/// images from data URIs. The frame around it adds `sandbox` without same-origin.
export const EVIDENCE_CSP =
  "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; font-src data:";

/// An HTML or SVG file as the `srcdoc` of a sandboxed frame, with the policy as its first element.
export function sandboxedDocument(source: string, mediaType: string): string {
  const policy = `<meta http-equiv="Content-Security-Policy" content="${EVIDENCE_CSP}">`;
  if (mediaType === 'image/svg+xml') {
    return `<!doctype html><html><head>${policy}<style>html,body{margin:0}svg{max-width:100%;height:auto}</style></head><body>${source}</body></html>`;
  }
  return `${policy}${source}`;
}

/// "in 1h 20m", "in 45s", or "expired", from RFC 3339 instants.
export function remaining(expiresAt: string, now: number): string {
  const left = Math.floor((Date.parse(expiresAt) - now) / 1000);
  if (Number.isNaN(left) || left <= 0) return 'expired';
  const h = Math.floor(left / 3600);
  const m = Math.floor((left % 3600) / 60);
  const s = left % 60;
  if (h > 0) return `in ${String(h)}h ${String(m)}m`;
  if (m > 0) return `in ${String(m)}m`;
  return `in ${String(s)}s`;
}

/// The requests in `open` whose ids have not been seen, in order.
export function unseen<T extends { id: string }>(seen: ReadonlySet<string>, open: readonly T[]): T[] {
  return open.filter((d) => !seen.has(d.id));
}

/// A browser notification for one request, unless the page lacks permission or an API.
export function notifyDecision(
  decision: { id: string; task: string; launch_key?: string | null },
  open: (id: string) => void,
): boolean {
  if (typeof Notification === 'undefined' || Notification.permission !== 'granted') return false;
  const n = new Notification('Decision needed', {
    body: decision.launch_key ? `${decision.task} · ${decision.launch_key}` : decision.task,
    tag: `decision-${decision.id}`,
  });
  n.onclick = () => {
    window.focus();
    open(decision.id);
  };
  return true;
}

/// The tone an answer label is drawn in: green for going ahead, red for stopping, plain otherwise.
export type LabelTone = 'go' | 'stop' | 'neutral';

const GO = new Set(['approve', 'approved', 'yes', 'accept', 'launch', 'go', 'ship', 'proceed']);
const STOP = new Set(['deny', 'denied', 'no', 'reject', 'shelve', 'stop', 'cancel', 'abort']);

export function labelTone(label: string): LabelTone {
  const key = label.toLowerCase();
  if (GO.has(key)) return 'go';
  if (STOP.has(key)) return 'stop';
  return 'neutral';
}
