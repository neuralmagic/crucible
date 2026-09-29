import { expect, test, type Page, type Route } from '@playwright/test';
import { PLAYBOOK_RUN, ROUTES, stubApi } from './api';

async function ready(page: Page, path: string): Promise<void> {
  const crashes: string[] = [];
  page.on('pageerror', (e) => crashes.push(String(e)));
  await page.clock.setFixedTime(new Date('2026-08-24T12:00:00Z'));
  await page.goto(path);
  await page.waitForLoadState('networkidle');
  expect(crashes, `uncaught exceptions on ${path}:\n${crashes.join('\n')}`).toEqual([]);
  await expect(page.locator('main')).toBeVisible();
}

/// Serve the overview with today's spend at `spent`, the fixture's ceiling kept.
async function spendAt(page: Page, spent: number): Promise<void> {
  const overview = ROUTES['/api/overview'] as Record<string, unknown>;
  await page.route('**/api/overview', (route: Route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ ...overview, cost_today: { current: spent, ceiling: 1500 } }),
    }),
  );
}

const strip = (page: Page) => page.locator('header + dl');

test.describe('the shared budget', () => {
  test('the masthead shows the shared budget and the caller spend', async ({ page }) => {
    await stubApi(page);
    await page.route('**/api/playbook-runs', (route: Route) =>
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify([
          { ...PLAYBOOK_RUN, key: 'playbook:survey:1', created_by: 'wren', created_at: '2026-08-24T08:00:00Z', cost_usd: 1.25 },
          { ...PLAYBOOK_RUN, key: 'playbook:survey:2', created_by: 'wren', created_at: '2026-08-24T09:00:00Z', cost_usd: 2 },
          { ...PLAYBOOK_RUN, key: 'playbook:survey:3', created_by: 'wren', created_at: '2026-08-23T09:00:00Z', cost_usd: 40 },
          { ...PLAYBOOK_RUN, key: 'playbook:survey:4', created_by: 'kylesayrs', created_at: '2026-08-24T09:00:00Z', cost_usd: 7 },
        ]),
      }),
    );
    await ready(page, '/');

    await expect(strip(page).getByText('Shared budget', { exact: true })).toBeVisible();
    await expect(strip(page).getByText('$412.80 / $1,500.00')).toBeVisible();
    await expect(strip(page).getByText('28%')).toBeVisible();
    await expect(strip(page).getByText('You today')).toBeVisible();
    await expect(strip(page).getByText('$3.25')).toBeVisible();

  });

  test('home says launches are waiting once the budget is spent', async ({ page }) => {
    await stubApi(page);
    await spendAt(page, 1500);
    await ready(page, '/');

    await expect(
      page.getByRole('link', { name: 'Shared budget spent. Launches resume in 12h.' }),
    ).toBeVisible();
  });

  test('home stays quiet about the budget below the ceiling', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/');

    await expect(page.getByText(/Shared budget spent/)).toHaveCount(0);
  });

  test('the launch form says when a spent budget lets launches resume', async ({ page }) => {
    await stubApi(page);
    await spendAt(page, 1600);
    await ready(page, '/playbooks/survey/launch');

    await expect(page.locator('main').getByText('Shared budget', { exact: true })).toBeVisible();
    await expect(page.locator('main').getByText('Spent. Launches resume in 12h.')).toBeVisible();
  });
});
