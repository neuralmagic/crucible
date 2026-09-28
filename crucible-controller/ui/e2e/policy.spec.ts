import { expect, test, type Page } from '@playwright/test';
import { stubApi } from './api';

async function ready(page: Page, path: string): Promise<void> {
  const crashes: string[] = [];
  page.on('pageerror', (e) => crashes.push(String(e)));
  await page.goto(path);
  await page.waitForLoadState('networkidle');
  expect(crashes, `uncaught exceptions on ${path}:\n${crashes.join('\n')}`).toEqual([]);
  await expect(page.locator('main')).toBeVisible();
}

async function appendToEditor(page: Page, text: string): Promise<void> {
  const editor = page.getByTestId('policy-editor');
  await expect(editor.locator('.view-lines')).toContainText('operators-access-autoresearch');
  await editor.locator('.view-lines').click();
  await page.keyboard.press('ControlOrMeta+End');
  await page.keyboard.insertText(text);
}

const GRANT = `
@id("llm-d-devs-autoresearch")
permit(principal, action == Action::"autoresearch:access", resource)
when { principal.hasTag("group:inference-eng-llm-d-devs") };
`;

async function explain(page: Page, login: string): Promise<void> {
  await page.getByLabel('Login').fill(login);
  await page.getByRole('button', { name: 'EXPLAIN' }).click();
}

test.describe('policy', () => {
  test('an administrator reads the active set and its history, newest first', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/policy');

    await expect(page.getByTestId('category-rail').getByRole('link', { name: 'Policy' })).toBeVisible();
    const rules = page.getByTestId('active-rules').locator('li');
    await expect(rules).toHaveCount(2);
    await expect(rules.nth(0)).toContainText('platform-admin-all');
    await expect(rules.nth(1)).toContainText('operators-access-autoresearch');
    await expect(rules.nth(1)).toContainText('hasTag("team:platform-operators")');

    await page.getByLabel('Filter rules').fill('operators');
    await expect(rules).toHaveCount(1);
    await page.getByLabel('Filter rules').fill('');

    const history = page.getByTestId('policy-history').locator('tbody tr');
    await expect(history).toHaveCount(2);
    await expect(history.nth(0)).toContainText('a1b2c3d4e5f6');
    await expect(history.nth(0)).toContainText('active');
    await expect(history.nth(1)).toContainText('ffeeddccbbaa');
  });

  test('a refused save shows the server message verbatim', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/policy');
    await appendToEditor(page, '\n@id("bogus-rule")\nbogus;\n');
    await page.getByRole('button', { name: 'SAVE VERSION' }).click();
    const refusal = 'policy bogus-rule does not validate: unexpected token `bogus`';
    await expect(page.getByRole('alert').filter({ hasText: refusal })).toHaveText(refusal);
  });

  test('a saved version diffs against the active set and activates, and explain follows it', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/policy');

    await explain(page, 'reed');
    await expect(page.locator('main')).toContainText('denied');
    await expect(page.locator('main')).toContainText('no-rule');
    await expect(page.locator('main')).toContainText('/groups/inference-eng-llm-d-devs');
    await expect(page.locator('main')).toContainText('as of 2026-09-27T12:00:00Z');
    await expect(page.locator('main')).toContainText('llm-d · member');

    await explain(page, 'ghost');
    const missing = 'no one with login ghost has signed in here';
    await expect(page.getByRole('alert').filter({ hasText: missing })).toHaveText(missing);

    await appendToEditor(page, GRANT);
    await page.getByRole('button', { name: 'SAVE VERSION' }).click();
    const diff = page.getByTestId('rule-diff');
    await expect(diff).toContainText('added 1');
    await expect(diff).toContainText('llm-d-devs-autoresearch');
    await expect(diff).not.toContainText('removed');
    await page.locator('[data-testid^="activate-0123456789ab"]').click();

    const rules = page.getByTestId('active-rules').locator('li');
    await expect(rules).toHaveCount(3);
    await expect(page.getByTestId('rule-diff')).toHaveCount(0);
    await expect(page.getByTestId('policy-history').locator('tbody tr')).toHaveCount(3);

    await explain(page, 'reed');
    await expect(page.locator('main')).toContainText('allowed');
    await page.getByRole('button', { name: 'llm-d-devs-autoresearch' }).click();
    await expect(page.locator('#rule-llm-d-devs-autoresearch')).toHaveClass(/bg-hi/);
    await expect(page.locator('#rule-llm-d-devs-autoresearch')).toBeInViewport();
  });

  test('an older set rolls back from its history row', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/policy');
    const history = page.getByTestId('policy-history');
    await history.locator('tbody tr').nth(1).getByRole('button').first().click();
    await expect(history.getByTestId('rule-diff')).toContainText('removed 1');
    await expect(history.getByTestId('rule-diff')).toContainText('operators-access-autoresearch');
    await history.getByRole('button', { name: 'ACTIVATE' }).click();
    await expect(page.getByTestId('active-rules').locator('li')).toHaveCount(1);
  });

  test('a caller who is not a platform administrator gets no rail entry and no page', async ({ page }) => {
    await stubApi(page);
    await page.route('**/api/whoami', (route) =>
      route.fulfill({
        json: {
          user: 'reed',
          admin: false,
          role: 'viewer',
          groups: [],
          mode: 'native',
          downgraded: false,
          proves_groups: true,
          entitlements: [],
          teams: [],
        },
      }),
    );
    await ready(page, '/policy');
    await expect(page.getByTestId('category-rail').getByRole('link', { name: 'Policy' })).toHaveCount(0);
    await expect(page.locator('main')).toContainText('PLATFORM ADMINISTRATORS ONLY');
  });
});
