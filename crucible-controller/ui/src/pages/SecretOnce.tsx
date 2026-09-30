import { useState } from 'react';
import { Button, Mono } from '../ui';
import { MetaRow } from './formControls';

export interface SecretOnceProps {
  secret: string;
  /// The full delivery URL when the controller knows its public delivery address.
  url: string | null;
  /// The delivery path, for when it does not.
  path: string;
  verifier: string;
}

export function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <Button
      variant="quiet"
      className="uppercase"
      onClick={() => {
        void navigator.clipboard.writeText(text).then(() => {
          setCopied(true);
        });
      }}
    >
      {copied ? 'Copied' : 'Copy'}
    </Button>
  );
}

/// A webhook secret, shown the one time the controller discloses it, with what a sender is
/// configured with.
export function SecretOnce({ secret, url, path, verifier }: SecretOnceProps) {
  const address = url ?? (verifier === 'path_token' ? `${path}/${secret}` : path);
  return (
    <div role="status" className="grid gap-2 border border-amber bg-sunk px-4.5 py-3">
      <Mono size="label" weight="semibold" uppercase tone="amber" className="tracking-section">
        Shown once. Copy it now; the controller keeps no readable copy.
      </Mono>
      <MetaRow label={url === null ? 'Delivery path' : 'Delivery URL'}>
        <span className="flex items-center gap-2">
          <Mono size="data" className="break-all">
            {address}
          </Mono>
          <CopyButton text={address} />
        </span>
      </MetaRow>
      <MetaRow label={verifier === 'path_token' ? 'Token' : 'HMAC secret'}>
        <span className="flex items-center gap-2">
          <Mono size="data" className="break-all">
            {secret}
          </Mono>
          <CopyButton text={secret} />
        </span>
      </MetaRow>
      {verifier === 'path_token' ? (
        <Mono size="data" tone="ink-3">
          Have the sender POST JSON to the delivery URL above; the token in it is the credential. On
          quay.io that is a "Webhook POST" notification.
        </Mono>
      ) : (
        <Mono size="data" tone="ink-3">
          Have the sender POST JSON to the delivery URL, signed with the HMAC secret. On GitHub that is a
          webhook with content type application/json.
        </Mono>
      )}
    </div>
  );
}
