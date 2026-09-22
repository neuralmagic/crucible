import { useState } from 'react';
import { AlertDialog } from '@base-ui-components/react/alert-dialog';
import { $api } from './api/client';
import {
  ALERT_POPUP,
  Button,
  DIALOG_BACKDROP,
  DIALOG_TITLE,
  Mono,
  Section,
  SectionBody,
  SectionHeader,
  Spinner,
  Status,
} from './ui';

interface AuditLine {
  label: string;
  value: string;
}

export function AutopilotToggle() {
  const whoami = $api.useQuery('get', '/api/whoami');
  const autopilot = $api.useQuery('get', '/api/autopilot');
  const mutation = $api.useMutation('post', '/api/autopilot');

  const [modalOpen, setModalOpen] = useState(false);
  const [reason, setReason] = useState('');

  const isAdmin = whoami.data?.role === 'admin';
  if (!isAdmin) return null;

  if (autopilot.isPending) return <Spinner />;
  if (autopilot.isError) return null;

  const { data } = autopilot;
  const nextEnabled = !data.enabled;

  const handleToggleClick = () => {
    setReason('');
    setModalOpen(true);
  };

  const handleConfirm = async () => {
    await mutation.mutateAsync({
      body: { enabled: nextEnabled, reason },
    });
    setModalOpen(false);
    setReason('');
    void autopilot.refetch();
  };

  const verb = nextEnabled ? 'Enable' : 'Disable';

  const audit: AuditLine[] = [];
  if (data.changed_by) audit.push({ label: 'Changed by', value: data.changed_by });
  if (data.changed_at) audit.push({ label: 'At', value: new Date(data.changed_at).toLocaleString() });
  if (data.reason) audit.push({ label: 'Reason', value: data.reason });

  return (
    <>
      <Section>
        <SectionHeader title="Autopilot" />
        <SectionBody>
          <div className="flex flex-col gap-3">
            <div className="flex items-center gap-4">
              <Status status={data.enabled ? 'enabled' : 'disabled'} tone={data.enabled ? 'green' : 'amber'} />
              <Button onClick={handleToggleClick}>{verb.toUpperCase()}</Button>
            </div>
            {audit.length > 0 ? (
              <dl className="m-0 grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1">
                {audit.map((line) => (
                  <div key={line.label} className="contents">
                    <dt>
                      <Mono size="label" tone="ink-3" uppercase>
                        {line.label}
                      </Mono>
                    </dt>
                    <dd className="m-0">
                      <Mono>{line.value}</Mono>
                    </dd>
                  </div>
                ))}
              </dl>
            ) : null}
          </div>
        </SectionBody>
      </Section>

      <AlertDialog.Root
        open={modalOpen}
        onOpenChange={(open) => {
          if (!open) setModalOpen(false);
        }}
      >
        <AlertDialog.Portal>
          <AlertDialog.Backdrop className={DIALOG_BACKDROP} />
          <AlertDialog.Popup className={ALERT_POPUP}>
            <AlertDialog.Title className={DIALOG_TITLE}>
              {verb} autopilot?
            </AlertDialog.Title>
            <AlertDialog.Description className="m-0 px-4 pt-3.5 text-ink-2">
              {nextEnabled
                ? 'Machine-initiated reconcile (rank, scope, launch) will resume.'
                : 'Machine-initiated reconcile will pause. Human overrides, completions, and discovery keep running.'}
            </AlertDialog.Description>
            <div className="px-4 pt-3.5 pb-4">
              <label
                htmlFor="autopilot-reason"
                className="block font-mono text-label uppercase tracking-label text-ink-3"
              >
                Reason
              </label>
              <textarea
                id="autopilot-reason"
                value={reason}
                onChange={(event) => {
                  setReason(event.target.value);
                }}
                placeholder="Why are you changing this?"
                autoFocus
                rows={3}
                className="mt-1 block w-full border border-rule-hard bg-paper px-2 py-1.5 font-mono text-data text-ink placeholder:text-ink-3"
              />
            </div>
            <div className="flex justify-end border-t border-rule">
              <Button
                onClick={() => {
                  setModalOpen(false);
                }}
              >
                CANCEL
              </Button>
              <Button
                variant="filled"
                onClick={() => void handleConfirm()}
                disabled={!reason.trim() || mutation.isPending}
              >
                {verb.toUpperCase()}
              </Button>
            </div>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog.Root>
    </>
  );
}
