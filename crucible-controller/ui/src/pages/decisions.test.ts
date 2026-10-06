import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  EVIDENCE_CSP,
  decodeText,
  fileView,
  labelTone,
  notifyDecision,
  parseCsv,
  remaining,
  sandboxedDocument,
  unseen,
} from './decisions';

describe('fileView', () => {
  it('maps each media type to how it is shown', () => {
    expect(fileView('image/png')).toBe('image');
    expect(fileView('image/svg+xml')).toBe('sandboxed');
    expect(fileView('text/html')).toBe('sandboxed');
    expect(fileView('text/markdown')).toBe('markdown');
    expect(fileView('text/csv')).toBe('csv');
    expect(fileView('application/json')).toBe('json');
    expect(fileView('text/plain')).toBe('text');
    expect(fileView('application/octet-stream')).toBe('download');
  });
});

describe('decodeText', () => {
  it('decodes UTF-8 beyond ASCII', () => {
    const base64 = btoa(String.fromCharCode(...new TextEncoder().encode('GPU × 8 → $412')));
    expect(decodeText(base64)).toBe('GPU × 8 → $412');
  });
});

describe('parseCsv', () => {
  it('splits rows and fields, honouring quotes', () => {
    expect(parseCsv('a,b\n1,"x, y"\r\n2,"say ""hi"""\n')).toEqual([
      ['a', 'b'],
      ['1', 'x, y'],
      ['2', 'say "hi"'],
    ]);
  });

  it('keeps a quoted newline and a last row without one', () => {
    expect(parseCsv('k,v\n"multi\nline",1')).toEqual([
      ['k', 'v'],
      ['multi\nline', '1'],
    ]);
    expect(parseCsv('')).toEqual([]);
  });
});

describe('sandboxedDocument', () => {
  it('leads every document with the no-network policy', () => {
    const cases: [string, string][] = [
      ['<h1>chart</h1><script>draw()</script>', 'text/html'],
      ['<svg viewBox="0 0 1 1"></svg>', 'image/svg+xml'],
    ];
    for (const [source, type] of cases) {
      const doc = sandboxedDocument(source, type);
      expect(doc).toContain(`content="${EVIDENCE_CSP}"`);
      expect(doc.indexOf('Content-Security-Policy')).toBeLessThan(doc.indexOf(source));
    }
    expect(EVIDENCE_CSP).toContain("default-src 'none'");
    expect(EVIDENCE_CSP).not.toContain('connect-src');
  });
});

describe('remaining', () => {
  const now = Date.parse('2026-10-06T12:00:00Z');
  it('counts down in the largest units that matter', () => {
    expect(remaining('2026-10-06T13:20:00Z', now)).toBe('in 1h 20m');
    expect(remaining('2026-10-06T12:05:30Z', now)).toBe('in 5m');
    expect(remaining('2026-10-06T12:00:45Z', now)).toBe('in 45s');
    expect(remaining('2026-10-06T11:59:59Z', now)).toBe('expired');
    expect(remaining('not a time', now)).toBe('expired');
  });
});

describe('unseen', () => {
  it('returns only the requests not seen before, in order', () => {
    const open = [{ id: 'a' }, { id: 'b' }, { id: 'c' }];
    expect(unseen(new Set(['b']), open).map((d) => d.id)).toEqual(['a', 'c']);
    expect(unseen(new Set(['a', 'b', 'c']), open)).toEqual([]);
  });
});

describe('notifyDecision', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('raises one notification that opens the request when permitted', () => {
    class FakeNotification {
      static permission = 'granted';
      static made: FakeNotification[] = [];
      onclick: (() => void) | null = null;
      constructor(
        readonly title: string,
        readonly options?: NotificationOptions,
      ) {
        FakeNotification.made.push(this);
      }
    }
    const made = FakeNotification.made;
    vi.stubGlobal('Notification', FakeNotification);
    const focus = vi.fn();
    vi.stubGlobal('window', { focus });
    const opened: string[] = [];
    expect(notifyDecision({ id: 'd1', task: 'gate', launch_key: 'pb#1' }, (id) => opened.push(id))).toBe(true);
    expect(made).toHaveLength(1);
    expect(made[0]?.options?.body).toBe('gate · pb#1');
    expect(made[0]?.options?.tag).toBe('decision-d1');
    made[0]?.onclick?.();
    expect(opened).toEqual(['d1']);
    expect(focus).toHaveBeenCalled();
  });

  it('stays quiet without permission or without the API', () => {
    vi.stubGlobal('Notification', { permission: 'default' });
    expect(notifyDecision({ id: 'd1', task: 'gate' }, () => undefined)).toBe(false);
    vi.stubGlobal('Notification', undefined);
    expect(notifyDecision({ id: 'd1', task: 'gate' }, () => undefined)).toBe(false);
  });
});

describe("labelTone", () => {
  it("reads going ahead as go, stopping as stop, and anything else as neutral", () => {
    for (const label of ["approve", "Yes", "launch"]) expect(labelTone(label)).toBe("go");
    for (const label of ["deny", "NO", "shelve"]) expect(labelTone(label)).toBe("stop");
    for (const label of ["scheduler", "maybe", ""]) expect(labelTone(label)).toBe("neutral");
  });
});
