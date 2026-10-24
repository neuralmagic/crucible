import { expect, test, type Page } from '@playwright/test';
import { CIMD_CONSENT_ID, CONSENT_ID, stubApi } from './api';

async function consent(page: Page, id = CONSENT_ID): Promise<void> {
  const crashes: string[] = [];
  page.on('pageerror', (e) => crashes.push(String(e)));
  await stubApi(page);
  await page.goto(`/consent?id=${id}`);
  await page.waitForLoadState('networkidle');
  expect(crashes, `uncaught exceptions on /consent:\n${crashes.join('\n')}`).toEqual([]);
}

const pirate = (page: Page) => page.getByTestId('cluster-identity-pirate');
const allow = (page: Page) => page.getByRole('button', { name: 'Allow' });

test.describe('the consent page', () => {
  test('shows the client, server, scopes and login, and holds Allow until the cluster is linked', async ({ page }) => {
    await consent(page);
    await expect(page.getByRole('heading', { name: 'Allow Claude Code?' })).toBeVisible();
    const request = page.getByTestId('consent-request');
    await expect(page.getByTestId('consent-client')).toHaveText('Claude Code');
    await expect(request).toContainText('claude-code');
    await expect(page.getByTestId('consent-publisher')).toHaveCount(0);
    await expect(request).toContainText('http://localhost:8851/mcp');
    await expect(request).toContainText('kueue.read kueue.submit');
    await expect(request).toContainText('wren');
    await expect(allow(page)).toBeDisabled();
    await expect(page.getByTestId('consent-blocked')).toHaveText('Link your username on pirate first.');
  });

  test('links the cluster inline, lands back on the same request, and Allow sends the browser to the client', async ({ page }) => {
    await consent(page);
    await pirate(page).getByRole('button', { name: 'Link pirate' }).click();
    await expect(page).toHaveURL(new RegExp(`/consent\\?id=${CONSENT_ID}&linked=pirate$`));
    await expect(page.getByTestId('cluster-link-outcome')).toHaveText('Linked pirate.');
    await expect(pirate(page).getByTestId('kube-user')).toHaveText('wren-k8s');
    await allow(page).click();
    await expect(page).toHaveURL(/\/client-callback\?code=c1&state=st$/);
  });

  test('deny goes back to the client with access_denied', async ({ page }) => {
    await consent(page);
    await page.getByRole('button', { name: 'Deny' }).click();
    await expect(page).toHaveURL(/\/client-callback\?error=access_denied&state=st$/);
  });

  test('a metadata document client is named with the host that published it', async ({ page }) => {
    await consent(page, CIMD_CONSENT_ID);
    await expect(page.getByRole('heading', { name: 'Allow Codex (chatgpt.com)?' })).toBeVisible();
    await expect(page.getByTestId('consent-client')).toHaveText('Codex (chatgpt.com)');
    await expect(page.getByTestId('consent-request')).toContainText('https://chatgpt.com/oauth/codex/client.json');
    await expect(page.getByTestId('consent-publisher')).toContainText('chatgpt.com published this client');
  });

  test('an id with nothing parked says so', async ({ page }) => {
    await consent(page, 'nope');
    await expect(page.getByText('No sign-in request')).toBeVisible();
  });
});
