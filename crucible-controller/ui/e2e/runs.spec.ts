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

const table = (page: Page) => page.locator('main table');

/// The table row link to one launch.
const run = (page: Page, key: string) => table(page).locator(`a[href="/playbook-runs/${encodeURIComponent(key)}"]`);

test.describe('the playbook runs list', () => {
  test('shows status, playbook, who, when and cost, with the parked reason under the playbook', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/playbook-runs?drafts=shown');

    await expect(table(page).locator('thead th')).toHaveText(['Status', 'Playbook', 'Launched by', 'When', 'Cost', '']);
    await expect(
      table(page).getByText('secrets: the pack declares secret pr_token, and repo neuralmagic/crucible has no binding for it'),
    ).toBeVisible();
    await expect(run(page, 'playbook:studio:0196')).toHaveText('studiodraft v3');
    await expect(page.getByText('Deferred one-shots')).toHaveCount(0);
  });

  test('hides draft studio runs until asked for', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/playbook-runs');

    await expect(run(page, 'playbook:studio:0196')).toHaveCount(0);
    await page.getByRole('button', { name: /2 draft runs hidden/ }).click();

    await expect(page).toHaveURL(/drafts=shown/);
    await expect(run(page, 'playbook:studio:0196')).toBeVisible();

    await page.getByRole('button', { name: 'hide draft runs' }).click();
    await expect(run(page, 'playbook:studio:0196')).toHaveCount(0);
  });

  test('filters by status and keeps the filter in the URL', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/playbook-runs');

    const filters = page.locator('dl[aria-label="Filters"]');
    await filters.getByRole('button', { name: /^parked/ }).click();

    await expect(page).toHaveURL(/status=parked/);
    await expect(run(page, 'playbook:survey:0197')).toBeVisible();
    await expect(run(page, 'playbook:triage:0201')).toHaveCount(0);

    await page.reload();
    await expect(run(page, 'playbook:triage:0201')).toHaveCount(0);

    await page.getByRole('button', { name: 'Clear all' }).click();
    await expect(run(page, 'playbook:triage:0201')).toBeVisible();
  });

  test('picking the draft origin shows only draft runs', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/playbook-runs');

    await page.locator('dl[aria-label="Filters"]').getByRole('button', { name: /^draft studio/ }).click();

    await expect(run(page, 'playbook:studio:0196')).toBeVisible();
    await expect(run(page, 'playbook:survey:0199')).toHaveCount(0);
  });

  test('the personal view shows only what the user launched', async ({ page }) => {
    await stubApi(page);
    await page.addInitScript(() => {
      localStorage.setItem('crucible.owner.context', 'user:wren');
    });
    await ready(page, '/playbook-runs');

    await expect(run(page, 'playbook:survey:0199')).toBeVisible();
    await expect(run(page, 'playbook:triage:0201')).toHaveCount(0);
  });
});
