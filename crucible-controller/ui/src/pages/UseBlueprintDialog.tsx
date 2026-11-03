import { Dialog } from '@base-ui-components/react/dialog';
import type { components } from '../api/schema';
import { ALERT_POPUP, Button, DIALOG_BACKDROP, DIALOG_TITLE, Mono } from '../ui';
import { FormError, PasswordField, TextField } from './formControls';
import { reasonLabel, type UseForm } from './blueprints';

type Placeholder = components['schemas']['PlaceholderDto'];

export interface UseBlueprintDialogProps {
  id: string;
  open: boolean;
  form: UseForm;
  askToken: boolean;
  pending: boolean;
  error: string | null;
  /// The placeholders a finished use left; null until one finishes with any.
  placeholders: readonly Placeholder[] | null;
  onFormChange: (form: UseForm) => void;
  onOpenChange: (open: boolean) => void;
  onConfirm: () => void;
  onOpenDraft: () => void;
}

/// The form in front of `POST /api/blueprints/{id}/use`. A use that left placeholders stays open
/// on them, so the new draft's missing secrets are seen before it is opened.
export function UseBlueprintDialog({
  id,
  open,
  form,
  askToken,
  pending,
  error,
  placeholders,
  onFormChange,
  onOpenChange,
  onConfirm,
  onOpenDraft,
}: UseBlueprintDialogProps) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Backdrop className={DIALOG_BACKDROP} />
        <Dialog.Popup data-testid="blueprint-use" className={ALERT_POPUP}>
          <Dialog.Title className={DIALOG_TITLE}>Use blueprint {id}</Dialog.Title>
          {placeholders === null ? (
            <div className="grid gap-3.5 px-4 py-3.5">
              <TextField
                id="blueprint-use-to"
                label="Draft id"
                mono
                required
                autoFocus
                value={form.to}
                onChange={(to) => {
                  onFormChange({ ...form, to });
                }}
              />
              <TextField
                id="blueprint-use-description"
                label="Description"
                value={form.description}
                onChange={(description) => {
                  onFormChange({ ...form, description });
                }}
              />
              {askToken ? (
                <PasswordField
                  id="blueprint-use-vault-token"
                  label="Your Vault token"
                  value={form.vaultToken}
                  onChange={(vaultToken) => {
                    onFormChange({ ...form, vaultToken });
                  }}
                />
              ) : null}
              {error === null ? null : <FormError>{error}</FormError>}
            </div>
          ) : (
            <div className="grid gap-2 px-4 py-3.5" data-testid="blueprint-use-placeholders">
              <span className="font-mono text-label font-semibold uppercase tracking-group text-ink-3">
                To bind
              </span>
              <ul className="m-0 grid list-none gap-1 p-0">
                {placeholders.map((placeholder) => (
                  <li key={placeholder.declared_name} className="flex gap-3">
                    <Mono size="data">{placeholder.declared_name}</Mono>
                    <span className="text-ink-2">{reasonLabel(placeholder.reason)}</span>
                  </li>
                ))}
              </ul>
            </div>
          )}
          <div className="flex justify-end border-t border-rule">
            {placeholders === null ? (
              <>
                <Button
                  className="uppercase"
                  onClick={() => {
                    onOpenChange(false);
                  }}
                >
                  Cancel
                </Button>
                <Button
                  variant="filled"
                  className="uppercase"
                  onClick={onConfirm}
                  disabled={pending || form.to.trim().length === 0}
                >
                  {pending ? 'Creating…' : 'Use'}
                </Button>
              </>
            ) : (
              <Button variant="filled" className="uppercase" onClick={onOpenDraft}>
                {`Open ${form.to.trim()}`}
              </Button>
            )}
          </div>
        </Dialog.Popup>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
