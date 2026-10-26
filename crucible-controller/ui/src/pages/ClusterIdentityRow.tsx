import { useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import { Button, Status } from '../ui';
import type { StatusTone } from '../ui';
import { BareTextInput } from './formControls';
import { identityView, type ClusterIdentityDto, type IdentityState } from './clusterIdentitiesView';

const IDENTITY_TONE: Record<IdentityState, StatusTone> = {
  resolved: 'green',
  linked: 'green',
  unlinked: 'amber',
  override: 'grey',
};

/// One cluster: who the person is there, and, on a link cluster, the controls that prove or forget
/// it. A pasted token lives only in this row's state until it is sent, and is cleared either way.
export function ClusterIdentityRow({
  identity,
  readOnly,
  returnTo,
}: {
  identity: ClusterIdentityDto;
  readOnly: boolean;
  /** The in-app page the cluster's sign-in lands back on; Settings when absent. */
  returnTo?: string;
}) {
  const qc = useQueryClient();
  const start = $api.useMutation('post', '/api/me/cluster-identities/{cluster}/link');
  const paste = $api.useMutation('put', '/api/me/cluster-identities/{cluster}');
  const unlink = $api.useMutation('delete', '/api/me/cluster-identities/{cluster}');
  const [pasting, setPasting] = useState(false);
  const [token, setToken] = useState('');
  const view = identityView(identity);
  const path = { params: { path: { cluster: identity.cluster } } };
  const failure = start.error ?? paste.error ?? unlink.error;
  const busy = start.isPending || paste.isPending || unlink.isPending;

  return (
    <div
      data-testid={`cluster-identity-${identity.cluster}`}
      className="flex items-start justify-between gap-4 border-t border-line py-3"
    >
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-mono text-data text-ink-1">{identity.cluster}</span>
          <Status status={view.headline} tone={IDENTITY_TONE[view.state]} pulse={false} />
          <span className="text-body text-ink-3">{view.rule}</span>
        </div>
        <p className="mt-1 font-mono text-data text-ink-2" data-testid="kube-user">
          {view.username}
        </p>
        <p className="mt-1 text-body text-ink-3">{view.detail}</p>
        {pasting && (
          <form
            className="mt-2 flex items-center gap-2"
            onSubmit={(event) => {
              event.preventDefault();
              const sent = token.trim();
              setToken('');
              paste.mutate(
                { ...path, body: { cluster_token: sent } },
                {
                  onSuccess: () => {
                    setPasting(false);
                    void qc.invalidateQueries();
                  },
                },
              );
            }}
          >
            <BareTextInput
              id={`cluster-token-${identity.cluster}`}
              type="password"
              mono
              value={token}
              onChange={setToken}
              placeholder="oc whoami -t, or an OIDC ID token"
              aria-label={`Token for ${identity.cluster}`}
            />
            <Button type="submit" variant="filled" className="uppercase" disabled={token.trim() === '' || busy}>
              {paste.isPending ? 'Checking…' : 'Link'}
            </Button>
          </form>
        )}
        {failure && <p className="mt-1 text-body text-red">{formatError(failure)}</p>}
      </div>
      {!readOnly && (
        <div className="flex shrink-0 gap-2">
          {view.link?.method === 'oauth' && (
            <Button
              className="uppercase"
              variant="filled"
              disabled={busy}
              onClick={() => {
                start.mutate(
                  { ...path, body: returnTo === undefined ? {} : { return_to: returnTo } },
                  {
                    onSuccess: (result) => {
                      window.location.assign(result.authorize_url);
                    },
                  },
                );
              }}
            >
              {start.isPending ? 'Opening…' : `${view.link.label} ${identity.cluster}`}
            </Button>
          )}
          {view.link?.method === 'paste' && !pasting && (
            <Button className="uppercase" variant="filled" disabled={busy} onClick={() => setPasting(true)}>
              {`${view.link.label} ${identity.cluster}`}
            </Button>
          )}
          {view.unlinkable && (
            <Button
              className="uppercase"
              variant="quiet"
              disabled={busy}
              onClick={() => {
                unlink.mutate(path, { onSuccess: () => void qc.invalidateQueries() });
              }}
            >
              {unlink.isPending ? 'Unlinking…' : 'Unlink'}
            </Button>
          )}
        </div>
      )}
    </div>
  );
}
