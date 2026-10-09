import { describe, expect, it } from 'vitest';
import { mcpSignInsShown, signedOutMessage } from './mcpSignInsView';

describe('mcpSignInsShown', () => {
  it('shows only where the controller signs MCP clients in', () => {
    expect(mcpSignInsShown({ enabled: false, sign_ins: [] })).toBe(false);
    expect(mcpSignInsShown({ enabled: true, sign_ins: [] })).toBe(true);
  });
});

describe('signedOutMessage', () => {
  it('counts the clients signed out, and pluralizes them', () => {
    expect(signedOutMessage(0)).toBe('No MCP client was signed in.');
    expect(signedOutMessage(1)).toMatch(/^Signed out 1 MCP client\./);
    expect(signedOutMessage(3)).toMatch(/^Signed out 3 MCP clients\./);
  });
});
