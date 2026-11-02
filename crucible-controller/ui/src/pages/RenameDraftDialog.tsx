import { Dialog } from '@base-ui-components/react/dialog';
import { ALERT_POPUP, Button, DIALOG_BACKDROP, DIALOG_TITLE } from '../ui';
import { FormError, TextField } from './formControls';

export interface RenameDraftDialogProps {
  id: string;
  open: boolean;
  to: string;
  pending: boolean;
  error: string | null;
  onToChange: (to: string) => void;
  onOpenChange: (open: boolean) => void;
  onConfirm: () => void;
}

/// The form in front of `POST /api/playbook-drafts/{id}/rename`.
export function RenameDraftDialog({
  id,
  open,
  to,
  pending,
  error,
  onToChange,
  onOpenChange,
  onConfirm,
}: RenameDraftDialogProps) {
  const unchanged = to.trim().length === 0 || to.trim() === id;
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Backdrop className={DIALOG_BACKDROP} />
        <Dialog.Popup data-testid="draft-rename" className={ALERT_POPUP}>
          <Dialog.Title className={DIALOG_TITLE}>Rename draft {id}</Dialog.Title>
          <div className="grid gap-3.5 px-4 py-3.5">
            <TextField id="draft-rename-to" label="New id" mono required autoFocus value={to} onChange={onToChange} />
            {error === null ? null : <FormError>{error}</FormError>}
          </div>
          <div className="flex justify-end border-t border-rule">
            <Button
              className="uppercase"
              onClick={() => {
                onOpenChange(false);
              }}
            >
              Cancel
            </Button>
            <Button variant="filled" className="uppercase" onClick={onConfirm} disabled={pending || unchanged}>
              {pending ? 'Renaming…' : 'Rename'}
            </Button>
          </div>
        </Dialog.Popup>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
