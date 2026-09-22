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

async function switchTo(page: Page, value: string): Promise<void> {
  await page.getByTestId('owner-switcher').click();
  await page.getByTestId('owner-switcher-menu').locator(`[data-value="${value}"]`).click();
}

test.describe('owner context', () => {
  test('the switcher lists the user and each team, and the lists follow it', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/playbooks');

    const rows = page.locator('main tbody tr');
    await expect(rows).toHaveCount(2);
    await expect(page.getByTestId('owner-switcher')).toContainText('ALL');

    await page.getByTestId('owner-switcher').click();
    const menu = page.getByTestId('owner-switcher-menu');
    await expect(menu.locator('[data-value]')).toHaveText([
      'ALL',
      'user:wren',
      'team:llm-dmaintainer',
      'team:platform-administratorsowner',
    ]);
    await menu.locator('[data-value="team:llm-d"]').click();

    await expect(page.getByTestId('owner-switcher')).toContainText('llm-d');
    await expect(rows).toHaveCount(1);
    await expect(rows.first()).toContainText('survey');
    await expect(page.locator('main')).toContainText('Showing 1');

    await switchTo(page, 'user:wren');
    await expect(rows).toHaveCount(1);
    await expect(rows.first()).toContainText('triage');

    // The context is per device: it survives navigation.
    await page.goto('/playbooks/drafts');
    await page.waitForLoadState('networkidle');
    await expect(page.getByTestId('owner-switcher')).toContainText('wren');
    await expect(page.locator('main tbody tr')).toHaveCount(1);
    await switchTo(page, 'team:llm-d');
    await expect(page.locator('main')).toContainText('NO DRAFTS');
    await expect(page.locator('main')).toContainText('Showing 0');
  });

  test('a creation form starts on the context and offers only principals that may own', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/playbooks/drafts');
    await switchTo(page, 'team:llm-d');

    const owner = page.locator('#draft-owner');
    await expect(owner).toHaveValue('team:llm-d');
    await expect(owner.locator('option')).toHaveText([
      'user:wren',
      'team:llm-d (maintainer)',
      'team:platform-administrators (owner)',
      'group:/groups/platform',
    ]);
    await owner.selectOption('user:wren');
    await expect(owner).toHaveValue('user:wren');

    await switchTo(page, 'all');
    await expect(owner).toHaveValue('user:wren');
  });

  test('a refused creation shows the decision verbatim', async ({ page }) => {
    await stubApi(page);
    await page.route('**/api/playbook-drafts', (route: Route) =>
      route.request().method() === 'POST'
        ? route.fulfill({
            status: 403,
            contentType: 'application/json',
            body: JSON.stringify({
              error: 'user:wren may not playbook_draft:create (no-rule)',
              rule: 'no-rule',
            }),
          })
        : route.fallback(),
    );
    await ready(page, '/playbooks/drafts');

    await page.locator('#draft-id').fill('mine');
    await page.locator('#draft-description').fill('a pack');
    await page.getByRole('button', { name: 'CREATE DRAFT' }).click();
    await expect(page.locator('main')).toContainText('user:wren may not playbook_draft:create (no-rule)');
  });
});

test.describe('teams', () => {
  test('settings shows every membership with how it is held', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/settings');

    const memberships = page.getByTestId('memberships');
    await expect(memberships).toContainText('team:llm-d');
    await expect(memberships).toContainText('maintainer');
    await expect(memberships).toContainText('group /groups/platform · maintainer');
    await expect(memberships).toContainText('rule configured-admins · owner');
  });

  test('a team page lists members by role with how each is held', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/teams');
    await expect(page.locator('main tbody tr')).toHaveCount(2);
    await page.locator('main tbody tr').first().getByRole('link', { name: 'llm-d' }).click();

    await expect(page).toHaveURL(/\/teams\/llm-d$/);
    await expect(page.locator('main')).toContainText('LLM-D');
    const cells = page.locator('main tbody tr');
    await expect(cells).toHaveCount(4);
    await expect(cells.nth(0)).toContainText('wren');
    await expect(cells.nth(0)).toContainText('direct');
    await expect(cells.nth(0)).toContainText('owner');
    await expect(cells.nth(1)).toContainText('/groups/platform');
    await expect(cells.nth(1)).toContainText('group');
    await expect(cells.nth(2)).toContainText('core');
    await expect(cells.nth(2)).toContainText('nested team');
    await expect(cells.nth(3)).toContainText('email-domain:example.com');
    await expect(cells.nth(3)).toContainText('rule');
  });
});

test.describe('team-centric views', () => {
  test('the rail lists each team, and a team page shows what it owns and acts as it', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/teams/llm-d');

    const rail = page.getByTestId('category-rail');
    await expect(rail.getByRole('link', { name: 'llm-d', exact: true })).toBeVisible();
    await expect(rail.getByRole('link', { name: 'platform-administrators', exact: true })).toBeVisible();
    await expect(rail.getByRole('link', { name: 'All teams', exact: true })).toBeVisible();

    const resources = page.getByTestId('team-resources');
    await expect(resources).toContainText('survey');
    await expect(resources).not.toContainText('triage');
    await expect(resources).toContainText('NO DRAFTS');

    await expect(page.getByTestId('act-as-team')).toHaveText('ACT AS');
    await page.getByTestId('act-as-team').click();
    await expect(page.getByTestId('act-as-team')).toHaveText('ACTING AS');
    await expect(page.getByTestId('owner-switcher')).toContainText('llm-d');

    await rail.getByRole('link', { name: 'Playbooks', exact: true }).first().click();
    await expect(page.locator('main tbody tr')).toHaveCount(1);
    await expect(page.locator('main tbody tr').first()).toContainText('survey');

    await page.goBack();
    await page.getByTestId('act-as-team').click();
    await expect(page.getByTestId('act-as-team')).toHaveText('ACT AS');
    await expect(page.getByTestId('owner-switcher')).toContainText('ALL');
  });
});
