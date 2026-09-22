import { expect, test, type Page, type Route } from '@playwright/test';
import { stubApi } from './api';

async function ready(page: Page, path: string): Promise<void> {
  const crashes: string[] = [];
  page.on('pageerror', (e) => crashes.push(String(e)));
  await page.goto(path);
  await page.waitForLoadState('networkidle');
  expect(crashes, `uncaught exceptions on ${path}:\n${crashes.join('\n')}`).toEqual([]);
  await expect(page.locator('main')).toBeVisible();
}

test.describe('shares', () => {
  test('a draft lists its shares with expiry, grants one, and revokes one', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/playbooks/drafts/studio');

    const rows = page.getByTestId('shares').locator('tbody tr');
    await expect(rows).toHaveCount(2);
    await expect(rows.nth(0)).toContainText('team:core');
    await expect(rows.nth(0)).toContainText('launcher');
    await expect(rows.nth(0)).toContainText('never');
    await expect(rows.nth(1)).toContainText('user:kylesayrs');
    await expect(rows.nth(1)).toContainText('expired 2026-01-01T00:00:00Z');

    await expect(page.getByRole('button', { name: 'SHARE', exact: true })).toBeDisabled();
    await page.locator('#share-grantee').fill('user:bob');
    await page.locator('#share-role').selectOption('editor');
    await page.locator('#share-until').fill('2026-12-31');
    await page.getByRole('button', { name: 'SHARE', exact: true }).click();
    await expect(rows).toHaveCount(3);
    const bob = rows.filter({ hasText: 'user:bob' });
    await expect(bob).toContainText('editor');
    await expect(bob).toContainText('2026-12-31T23:59:59Z');
    await expect(page.locator('#share-grantee')).toHaveValue('');

    await rows.nth(0).getByRole('button', { name: 'REVOKE' }).click();
    await expect(rows).toHaveCount(2);
    await expect(page.getByTestId('shares')).not.toContainText('team:core');
  });

  test('a refused grant shows the decision verbatim', async ({ page }) => {
    await stubApi(page);
    await page.route(
      (url: URL) => decodeURIComponent(url.pathname) === '/api/playbook-drafts/studio/shares/user:bob',
      (route: Route) =>
      route.fulfill({
        status: 403,
        contentType: 'application/json',
        body: JSON.stringify({
          error: 'an editor share confers playbook_draft:launch, which you do not hold on studio',
        }),
      }),
    );
    await ready(page, '/playbooks/drafts/studio');
    await page.locator('#share-grantee').fill('user:bob');
    await page.locator('#share-role').selectOption('editor');
    await page.getByRole('button', { name: 'SHARE', exact: true }).click();
    await expect(page.locator('main')).toContainText(
      'an editor share confers playbook_draft:launch, which you do not hold on studio',
    );
  });
});

test.describe('team members', () => {
  test('an owner adds a member, changes a role, and removes one', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/teams/llm-d');

    const rows = page.getByTestId('members').locator('tbody tr');
    await expect(rows).toHaveCount(4);

    await page.locator('#member-kind').selectOption('user');
    await page.locator('#member-name').fill('bob');
    await page.locator('#member-role').selectOption('maintainer');
    await page.getByRole('button', { name: 'ADD MEMBER' }).click();
    await expect(rows).toHaveCount(5);
    const bob = rows.filter({ hasText: 'bob' });
    await expect(bob).toContainText('maintainer');
    await expect(bob).toContainText('direct');

    await bob.getByRole('combobox', { name: 'Role of bob' }).selectOption('member');
    await expect(bob.getByRole('combobox', { name: 'Role of bob' })).toHaveValue('member');

    await bob.getByRole('button', { name: 'REMOVE' }).click();
    await expect(rows).toHaveCount(4);
    await expect(page.getByTestId('members')).not.toContainText('bob');
  });

  test('the last user at owner cannot be demoted', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/teams/llm-d');
    const rows = page.getByTestId('members').locator('tbody tr');
    await rows.nth(0).getByRole('combobox', { name: 'Role of wren' }).selectOption('member');
    await expect(page.locator('main')).toContainText('a team keeps at least one user at owner');
    await expect(rows.nth(0)).toContainText('owner');
  });
});
