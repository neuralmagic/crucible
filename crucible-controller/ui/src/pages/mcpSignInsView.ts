import type { components } from '../api/schema.d';

export type McpSignInsDto = components['schemas']['McpSignInsDto'];
export type McpSignInDto = components['schemas']['McpSignInDto'];

/** One client's live approvals, for Settings to list and sign out together. */
export interface ClientSignIns {
  clientId: string;
  label: string;
  signIns: McpSignInDto[];
}

/** The section shows only where the controller signs MCP clients in. */
export function mcpSignInsShown(dto: McpSignInsDto): boolean {
  return dto.enabled;
}

/// What a client is called: its name, and for a metadata document client the host that published
/// that name, so a self-described client cannot pass for a registered one.
export function clientLabel(client: { client_name: string; client_host?: string | null }): string {
  return client.client_host ? `${client.client_name} (${client.client_host})` : client.client_name;
}

/// The approvals grouped by client, clients in label order, each client's newest first as listed.
export function byClient(signIns: McpSignInDto[]): ClientSignIns[] {
  const groups = new Map<string, ClientSignIns>();
  for (const signIn of signIns) {
    const group = groups.get(signIn.client_id);
    if (group) {
      group.signIns.push(signIn);
    } else {
      groups.set(signIn.client_id, { clientId: signIn.client_id, label: clientLabel(signIn), signIns: [signIn] });
    }
  }
  return [...groups.values()].sort((a, b) => a.label.localeCompare(b.label) || a.clientId.localeCompare(b.clientId));
}

/// The sentence the sign-out result puts on screen, for every client or for the one named.
export function signedOutMessage(revoked: number, client?: string): string {
  if (client !== undefined) {
    if (revoked === 0) return `${client} was not signed in.`;
    const approvals = revoked === 1 ? 'approval' : 'approvals';
    return `Signed out ${client} (${revoked} ${approvals}). It signs in again the next time it needs a token.`;
  }
  if (revoked === 0) return 'No MCP client was signed in.';
  const clients = revoked === 1 ? 'client' : 'clients';
  return `Signed out ${revoked} MCP ${clients}. Each signs in again the next time it needs a token.`;
}
