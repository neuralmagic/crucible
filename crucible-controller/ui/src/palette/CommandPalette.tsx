import { Dialog } from '@base-ui-components/react/dialog';
import { Command as Cmdk } from 'cmdk';
import { useEffect, useMemo, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { apiClient } from '../api/client';
import { DIALOG_BACKDROP } from '../ui/dialogClasses';
import { score } from './match';
import { type Action, type Command, controllerSources, docsSource, type Source } from './sources';

type Loaded =
  | { group: string; state: 'loading' }
  | { group: string; state: 'ready'; commands: Command[] }
  | { group: string; state: 'failed'; error: string };

const fetchText = async (url: string): Promise<string> => {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${url}: ${res.status}`);
  return res.text();
};

/// The typed client answers an error status with `error` set rather than a rejection; a list that
/// failed must not read as an empty one.
async function ok(
  request: Promise<{ data?: unknown; error?: unknown; response: Response }>
): Promise<unknown> {
  const { data, error, response } = await request;
  if (error !== undefined || !response.ok) throw new Error(`HTTP ${response.status}`);
  return data;
}

/// The list reads go through the typed client, so the act-as header and the sign-in redirect
/// apply to them as to every other read.
function useSources(): Source[] {
  return useMemo(
    () => [
      ...controllerSources({
        playbooks: () => ok(apiClient.GET('/api/playbooks')),
        drafts: () => ok(apiClient.GET('/api/playbook-drafts')),
        launches: () => ok(apiClient.GET('/api/playbook-runs')),
      }),
      docsSource(fetchText),
    ],
    []
  );
}

/// Cmd/Ctrl+K anywhere opens a search over pages, playbooks, drafts, recent runs and the docs.
export function CommandPalette() {
  const [open, setOpen] = useState(false);
  const navigate = useNavigate();
  const sources = useSources();

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      // Monaco claims Cmd+K as a chord prefix and marks it handled.
      if (event.defaultPrevented) return;
      if (event.altKey || event.shiftKey || !(event.ctrlKey || event.metaKey)) return;
      if (event.key !== 'k' && event.key !== 'K') return;
      event.preventDefault();
      setOpen((o) => !o);
    };
    window.addEventListener('keydown', onKey);
    return () => {
      window.removeEventListener('keydown', onKey);
    };
  }, []);

  const run = (action: Action) => {
    setOpen(false);
    switch (action.kind) {
      case 'navigate':
        void navigate(action.path);
        break;
      case 'external':
        window.open(action.url, '_blank', 'noopener');
        break;
    }
  };

  return (
    <Dialog.Root open={open} onOpenChange={setOpen}>
      <Dialog.Portal>
        <Dialog.Backdrop className={DIALOG_BACKDROP} />
        <Dialog.Popup
          aria-label="Command palette"
          className="fixed top-[14vh] left-1/2 z-40 w-[min(680px,92vw)] -translate-x-1/2 border border-rule-hard bg-surface shadow-lg outline-none"
        >
          {open ? <Palette sources={sources} onAction={run} /> : null}
        </Dialog.Popup>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

const GROUP =
  '[&_[cmdk-group-heading]]:px-3.5 [&_[cmdk-group-heading]]:pt-2.5 [&_[cmdk-group-heading]]:pb-1 [&_[cmdk-group-heading]]:font-mono [&_[cmdk-group-heading]]:text-label [&_[cmdk-group-heading]]:font-semibold [&_[cmdk-group-heading]]:uppercase [&_[cmdk-group-heading]]:tracking-group [&_[cmdk-group-heading]]:text-ink-3';
const ITEM =
  'flex min-w-0 cursor-pointer items-baseline gap-2.5 px-3.5 py-1.5 font-mono text-data-lg text-ink select-none data-[selected=true]:bg-hi data-[disabled=true]:cursor-default data-[disabled=true]:text-ink-3';
const QUIET = 'px-3.5 py-2 font-mono text-data-lg text-ink-3';

/// Mounted only while the dialog is open, so every open reloads the lists.
function Palette({ sources, onAction }: { sources: Source[]; onAction: (action: Action) => void }) {
  const [groups, setGroups] = useState<Loaded[]>(() =>
    sources.map((s) => ({ group: s.group, state: 'loading' }))
  );

  useEffect(() => {
    let live = true;
    sources.forEach((source, index) => {
      void source
        .load()
        .then(
          (commands): Loaded => ({ group: source.group, state: 'ready', commands }),
          (err: unknown): Loaded => ({ group: source.group, state: 'failed', error: String(err) })
        )
        .then((loaded) => {
          if (live) setGroups((current) => current.map((g, i) => (i === index ? loaded : g)));
        });
    });
    return () => {
      live = false;
    };
  }, [sources]);

  return (
    <Cmdk
      label="Command palette"
      loop
      filter={(_value, search, keywords) => {
        const [title = '', ...rest] = keywords ?? [];
        return score(search, title, rest);
      }}
    >
      <Cmdk.Input
        autoFocus
        placeholder="Search"
        className="block w-full border-0 border-b border-rule bg-transparent px-3.5 py-3 text-lede text-ink outline-none placeholder:text-ink-3"
      />
      <Cmdk.List className="max-h-[min(440px,62vh)] overflow-y-auto overscroll-contain py-1">
        <Cmdk.Empty className={QUIET}>No matches</Cmdk.Empty>
        {groups.map((g) => (
          <Cmdk.Group key={g.group} heading={g.group} className={GROUP}>
            {g.state === 'loading' ? <Cmdk.Loading className={QUIET}>Loading…</Cmdk.Loading> : null}
            {g.state === 'failed' ? (
              <Cmdk.Item disabled value={`${g.group} failed`} className={ITEM}>
                Couldn't load: {g.error}
              </Cmdk.Item>
            ) : null}
            {g.state === 'ready'
              ? g.commands.map((c) => (
                  <Cmdk.Item
                    key={c.id}
                    value={c.id}
                    keywords={[c.title, ...(c.subtitle ? [c.subtitle] : []), ...c.keywords]}
                    onSelect={() => {
                      onAction(c.action);
                    }}
                    className={ITEM}
                  >
                    <span className="max-w-[60%] flex-none truncate">{c.title}</span>
                    {c.subtitle ? (
                      <span className="min-w-0 flex-1 truncate text-data tracking-data text-ink-3">
                        {c.subtitle}
                      </span>
                    ) : null}
                    {c.action.kind === 'external' ? (
                      <span className="ml-auto text-ink-3" aria-hidden>
                        ↗
                      </span>
                    ) : null}
                  </Cmdk.Item>
                ))
              : null}
          </Cmdk.Group>
        ))}
      </Cmdk.List>
    </Cmdk>
  );
}
