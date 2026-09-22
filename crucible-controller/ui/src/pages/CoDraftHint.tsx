import { useState } from 'react';
import { $api } from '../api/client';
import { useDeviceFlag } from '../useDeviceFlag';
import { Button, Section, SectionBody, SectionHeader } from '../ui';

/// Per device rather than per identity: whether this browser still needs the walkthrough is a
/// property of the machine the agent runs on, not of who is signed in.
export const CO_DRAFT_HINT_KEY = 'crucible.hint.coDraft';

function CommandBlock({ commands }: { commands: string[] }) {
  const [copied, setCopied] = useState(false);
  const text = commands.join('\n');
  return (
    <div className="group relative">
      <pre className="overflow-x-auto border border-rule bg-sunk px-3 py-2 font-mono text-data text-ink-2">
        {text}
      </pre>
      <Button
        className="absolute top-1.5 right-1.5 opacity-0 group-hover:opacity-100 focus-visible:opacity-100"
        onClick={() => {
          void navigator.clipboard.writeText(text).then(() => {
            setCopied(true);
            window.setTimeout(() => {
              setCopied(false);
            }, 1500);
          });
        }}
      >
        {copied ? 'COPIED' : 'COPY'}
      </Button>
    </div>
  );
}

/// The loop this studio is for: your own agent writes the pack, you read the saves. The commands
/// come from the controller so they name this deployment rather than an example one.
export function CoDraftHint() {
  const [dismissed, setDismissed] = useDeviceFlag(CO_DRAFT_HINT_KEY, false);
  const setup = $api.useQuery('get', '/api/playbooks/drafts/co-draft');

  if (dismissed || setup.data === undefined) return null;
  const { skill_url, steps, example_prompt } = setup.data;

  return (
    <Section>
      <SectionHeader
        title="Co-draft with your own agent"
        note="the studio holds the pack; your agent writes it"
        actions={
          <Button
            onClick={() => {
              setDismissed(true);
            }}
          >
            DISMISS
          </Button>
        }
      />
      <SectionBody className="grid gap-3">
        <p className="m-0 max-w-[80ch] text-data-lg text-ink-2">
          A draft is an append-only stack of saves, so a local Claude Code agent can author into one
          while you watch it compile. Download the skill file below — it is generated with this
          controller&apos;s URL in it — install it, and point the agent here.
        </p>
        <div>
          <Button variant="filled" render={<a href={skill_url} download />}>
            DOWNLOAD SKILL
          </Button>
        </div>
        {steps.map((step) => (
          <div key={step.label} className="grid gap-1.5">
            <div className="font-mono text-label font-semibold tracking-group text-ink-3 uppercase">
              {step.label}
            </div>
            <CommandBlock commands={step.commands} />
          </div>
        ))}
        <div className="grid gap-1.5">
          <div className="font-mono text-label font-semibold tracking-group text-ink-3 uppercase">
            Then ask for something
          </div>
          <p className="m-0 max-w-[80ch] border border-rule bg-sunk px-3 py-2 text-data-lg text-ink-2 italic">
            {example_prompt}
          </p>
        </div>
      </SectionBody>
    </Section>
  );
}
