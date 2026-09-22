import { AlertDialog } from '@base-ui-components/react/alert-dialog';
import { ALERT_POPUP, Button, DIALOG_BACKDROP, DIALOG_TITLE, Mono } from '../ui';
import { FormError } from './formControls';

export interface DeleteDraftDialogProps {
  id: string;
  open: boolean;
  pending: boolean;
  error: string | null;
  onOpenChange: (open: boolean) => void;
  onConfirm: () => void;
}

/// The confirm in front of `DELETE /api/playbook-drafts/{id}`. It names the draft, because every
/// version of it goes with the row and the rail is a list of near-identical slugs.
export function DeleteDraftDialog({
  id,
  open,
  pending,
  error,
  onOpenChange,
  onConfirm,
}: DeleteDraftDialogProps) {
  return (
    <AlertDialog.Root open={open} onOpenChange={onOpenChange}>
      <AlertDialog.Portal>
        <AlertDialog.Backdrop className={DIALOG_BACKDROP} />
        <AlertDialog.Popup data-testid="draft-delete-confirm" className={ALERT_POPUP}>
          <AlertDialog.Title className={DIALOG_TITLE}>
            Delete draft {id}?
          </AlertDialog.Title>
          <AlertDialog.Description className="m-0 px-4 py-3.5 text-ink-2">
            Drops <Mono size="data">{id}</Mono> and every saved version of it. Runs it already
            launched keep their own records; the pack source does not come back.
          </AlertDialog.Description>
          {error === null ? null : (
            <div className="px-4 pb-3.5">
              <FormError>{error}</FormError>
            </div>
          )}
          <div className="flex justify-end border-t border-rule">
            <Button
              onClick={() => {
                onOpenChange(false);
              }}
            >
              CANCEL
            </Button>
            <Button variant="filled" onClick={onConfirm} disabled={pending}>
              {pending ? 'DELETING…' : 'DELETE'}
            </Button>
          </div>
        </AlertDialog.Popup>
      </AlertDialog.Portal>
    </AlertDialog.Root>
  );
}
