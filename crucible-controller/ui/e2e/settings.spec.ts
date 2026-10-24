import { expect, test, type Page } from '@playwright/test';
import { GOOD_CLUSTER_TOKEN, stubApi } from './api';

async function settings(page: Page, query = ''): Promise<void> {
  const crashes: string[] = [];
  page.on('pageerror', (e) => crashes.push(String(e)));
  await stubApi(page);
  await page.goto(`/settings${query}`);
  await page.waitForLoadState('networkidle');
  expect(crashes, `uncaught exceptions on /settings:\n${crashes.join('\n')}`).toEqual([]);
}

const row = (page: Page, cluster: string) => page.getByTestId(`cluster-identity-${cluster}`);

test.describe('cluster identities', () => {
  test('every cluster shows its rule and username; SSO clusters offer no controls', async ({ page }) => {
    await settings(page);
    await expect(row(page, 'hub')).toContainText('SSO login');
    await expect(row(page, 'hub').getByTestId('kube-user')).toHaveText('wren');
    await expect(row(page, 'waldorf').getByTestId('kube-user')).toHaveText('wren@example.com');
    for (const cluster of ['hub', 'waldorf']) {
      await expect(row(page, cluster).getByRole('button')).toHaveCount(0);
    }
    await expect(row(page, 'pirate')).toContainText('NOT LINKED');
    await expect(row(page, 'pirate').getByRole('button', { name: 'Link pirate' })).toBeVisible();
  });

  test('linking signs in at the cluster and lands back linked', async ({ page }) => {
    await settings(page);
    await row(page, 'pirate').getByRole('button', { name: 'Link pirate' }).click();
    await expect(page).toHaveURL(/\/settings\?linked=pirate$/);
    await expect(page.getByTestId('cluster-link-outcome')).toHaveText('Linked pirate.');
    await expect(row(page, 'pirate').getByTestId('kube-user')).toHaveText('wren-k8s');
    await expect(row(page, 'pirate')).toContainText('Proven by signing in to the cluster');
    await expect(row(page, 'pirate').getByRole('button', { name: 'Relink pirate' })).toBeVisible();
  });

  test('a cluster without OAuth takes a pasted token, and a refused one says why', async ({ page }) => {
    await settings(page);
    await row(page, 'rig').getByRole('button', { name: 'Link rig' }).click();
    const field = row(page, 'rig').getByLabel('Token for rig');
    await expect(field).toHaveAttribute('type', 'password');
    await field.fill('sha256~nope');
    await row(page, 'rig').getByRole('button', { name: 'Link' }).click();
    await expect(row(page, 'rig')).toContainText('did not accept the presented token');
    await expect(field).toHaveValue('');
    await field.fill(GOOD_CLUSTER_TOKEN);
    await row(page, 'rig').getByRole('button', { name: 'Link' }).click();
    await expect(row(page, 'rig').getByTestId('kube-user')).toHaveText('wren-k8s');
    await expect(row(page, 'rig')).toContainText('TokenReview');
  });

  test('unlinking forgets the username', async ({ page }) => {
    await settings(page);
    await row(page, 'pirate').getByRole('button', { name: 'Link pirate' }).click();
    await expect(row(page, 'pirate').getByTestId('kube-user')).toHaveText('wren-k8s');
    await row(page, 'pirate').getByRole('button', { name: 'Unlink' }).click();
    await expect(row(page, 'pirate')).toContainText('NOT LINKED');
    await expect(row(page, 'pirate').getByRole('button', { name: 'Unlink' })).toHaveCount(0);
  });

  test('a failed callback names the problem in a fixed sentence, never the query text', async ({ page }) => {
    await settings(page, '?link_error=state-mismatch');
    await expect(page.getByTestId('cluster-link-outcome')).toHaveText(
      'The sign-in did not match the link this browser started. Start it again.',
    );
    await settings(page, '?link_error=Call%20555-0100');
    await expect(page.getByTestId('cluster-link-outcome')).toHaveText('The link did not complete. Start it again.');
  });
});

test('MCP sign-ins are listed per client, and one client signs out alone', async ({ page }) => {
  await settings(page);
  const codex = page.getByTestId('mcp-client-codex');
  const claude = page.getByTestId('mcp-client-claude-code');
  await expect(codex).toContainText('Codex');
  await expect(codex).toContainText('http://localhost:8852/mcp');
  await expect(claude).toContainText('Claude Code');
  await expect(claude).toContainText('http://localhost:8851/mcp');
  await expect(page.getByTestId('mcp-client-dyn-AAAAAAAAAAAAAAAAAAAAAAAA')).toContainText('pi (self-registered)');
  await page.getByRole('button', { name: 'Sign out Codex' }).click();
  await expect(page.getByText(/^Signed out Codex \(1 approval\)\./)).toBeVisible();
  await expect(codex).toHaveCount(0);
  await expect(claude).toContainText('http://localhost:8851/mcp');
});

test('signing out MCP clients ends every approval at once', async ({ page }) => {
  await settings(page);
  const list = page.getByTestId('mcp-sign-ins');
  await expect(list).toContainText('http://localhost:8851/mcp');
  await page.getByRole('button', { name: 'Sign out MCP clients' }).click();
  await expect(page.getByText(/^Signed out 3 MCP clients\./)).toBeVisible();
  await expect(page.getByText('No MCP client is signed in.')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Sign out MCP clients' })).toBeDisabled();
});
