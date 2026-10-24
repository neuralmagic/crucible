import type { components } from '../api/schema.d';

export type McpSignInsDto = components['schemas']['McpSignInsDto'];

/** The section shows only where the controller signs MCP clients in. */
export function mcpSignInsShown(dto: McpSignInsDto): boolean {
  return dto.enabled;
}

/// The sentence the sign-out result puts on screen.
export function signedOutMessage(revoked: number): string {
  if (revoked === 0) return 'No MCP client was signed in.';
  const clients = revoked === 1 ? 'client' : 'clients';
  return `Signed out ${revoked} MCP ${clients}. Each signs in again the next time it needs a token.`;
}
