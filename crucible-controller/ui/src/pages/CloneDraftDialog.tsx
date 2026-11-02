import { Dialog } from '@base-ui-components/react/dialog';
import type { components } from '../api/schema';
import { ALERT_POPUP, Button, DIALOG_BACKDROP, DIALOG_TITLE, Mono } from '../ui';
import { FormError, PasswordField, TextField } from './formControls';
import type { CloneForm } from './draftStudio';

type LeftBinding = components['schemas']['LeftBindingDto'];

export interface CloneDraftDialogProps {
  id: string;
  open: boolean;
  form: CloneForm;
  pending: boolean;
  error: string | null;
  /// The bindings a finished clone left behind; null until one finishes with any.
  left: readonly LeftBinding[] | null;
  onFormChange: (form: CloneForm) => void;
  onOpenChange: (open: boolean) => void;
  onConfirm: () => void;
  onOpenCopy: () => void;
}

/// The form in front of `POST /api/playbook-drafts/{id}/clone`. A clone that could not copy every
/// binding stays open on the list of what it left, so nothing is dropped unseen.
export function CloneDraftDialog({
  id,
  open,
  form,
  pending,
  error,
  left,
  onFormChange,
  onOpenChange,
  onConfirm,
  onOpenCopy,
}: CloneDraftDialogProps) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Backdrop className={DIALOG_BACKDROP} />
        <Dialog.Popup data-testid="draft-clone" className={ALERT_POPUP}>
          <Dialog.Title className={DIALOG_TITLE}>Clone draft {id}</Dialog.Title>
          {left === null ? (
            <div className="grid gap-3.5 px-4 py-3.5">
              <TextField
                id="draft-clone-to"
                label="New id"
                mono
                required
                autoFocus
                value={form.to}
                onChange={(to) => {
                  onFormChange({ ...form, to });
                }}
              />
              <TextField
                id="draft-clone-description"
                label="Description"
                value={form.description}
                onChange={(description) => {
                  onFormChange({ ...form, description });
                }}
              />
              <PasswordField
                id="draft-clone-vault-token"
                label="Your Vault token"
                value={form.vaultToken}
                onChange={(vaultToken) => {
                  onFormChange({ ...form, vaultToken });
                }}
              />
              {error === null ? null : <FormError>{error}</FormError>}
            </div>
          ) : (
            <div className="grid gap-2 px-4 py-3.5" data-testid="draft-clone-left">
              <span className="font-mono text-label font-semibold uppercase tracking-group text-ink-3">
                Not copied
              </span>
              <ul className="m-0 grid list-none gap-1 p-0">
                {left.map((binding) => (
                  <li key={binding.declared_name} className="flex gap-3">
                    <Mono size="data">{binding.declared_name}</Mono>
                    <span className="text-ink-2">{binding.reason}</span>
                  </li>
                ))}
              </ul>
            </div>
          )}
          <div className="flex justify-end border-t border-rule">
            {left === null ? (
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
                  {pending ? 'Cloning…' : 'Clone'}
                </Button>
              </>
            ) : (
              <Button variant="filled" className="uppercase" onClick={onOpenCopy}>
                {`Open ${form.to.trim()}`}
              </Button>
            )}
          </div>
        </Dialog.Popup>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
