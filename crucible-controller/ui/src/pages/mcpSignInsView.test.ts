import { describe, expect, it } from 'vitest';
import { byClient, clientLabel, mcpSignInsShown, signedOutMessage, type McpSignInDto } from './mcpSignInsView';

function signIn(client_id: string, client_name: string, resource: string, client_host: string | null = null): McpSignInDto {
  return {
    client_id,
    client_name,
    client_host,
    resource,
    cluster: 'pirate',
    approved_at: '2026-10-09T10:00:00Z',
    last_used_at: '2026-10-09T11:00:00Z',
  };
}

describe('mcpSignInsShown', () => {
  it('shows only where the controller signs MCP clients in', () => {
    expect(mcpSignInsShown({ enabled: false, sign_ins: [] })).toBe(false);
    expect(mcpSignInsShown({ enabled: true, sign_ins: [] })).toBe(true);
  });
});

describe('clientLabel', () => {
  it('is the name of a registered client', () => {
    expect(clientLabel({ client_name: 'Codex', client_host: null })).toBe('Codex');
    expect(clientLabel({ client_name: 'Codex' })).toBe('Codex');
  });

  it('carries the publishing host for a metadata document client', () => {
    expect(clientLabel({ client_name: 'Codex', client_host: 'chatgpt.com' })).toBe('Codex (chatgpt.com)');
  });
});

describe('byClient', () => {
  it('groups each client’s approvals under one label, clients in label order', () => {
    const groups = byClient([
      signIn('codex', 'Codex', 'http://localhost:8851/mcp'),
      signIn('claude-code', 'Claude Code', 'http://localhost:8851/mcp'),
      signIn('codex', 'Codex', 'http://localhost:8852/mcp'),
      signIn('https://chatgpt.com/oauth/codex/client.json', 'Codex', 'http://localhost:8851/mcp', 'chatgpt.com'),
    ]);
    expect(groups.map((g) => [g.clientId, g.label, g.signIns.length])).toEqual([
      ['claude-code', 'Claude Code', 1],
      ['codex', 'Codex', 2],
      ['https://chatgpt.com/oauth/codex/client.json', 'Codex (chatgpt.com)', 1],
    ]);
    expect(groups[1].signIns.map((s) => s.resource)).toEqual(['http://localhost:8851/mcp', 'http://localhost:8852/mcp']);
  });

  it('is empty without approvals', () => {
    expect(byClient([])).toEqual([]);
  });
});

describe('signedOutMessage', () => {
  it('counts the clients signed out, and pluralizes them', () => {
    expect(signedOutMessage(0)).toBe('No MCP client was signed in.');
    expect(signedOutMessage(1)).toMatch(/^Signed out 1 MCP client\./);
    expect(signedOutMessage(3)).toMatch(/^Signed out 3 MCP clients\./);
  });

  it('names the one client signed out', () => {
    expect(signedOutMessage(0, 'Codex')).toBe('Codex was not signed in.');
    expect(signedOutMessage(1, 'Codex')).toMatch(/^Signed out Codex \(1 approval\)\./);
    expect(signedOutMessage(2, 'pi')).toMatch(/^Signed out pi \(2 approvals\)\./);
  });
});
