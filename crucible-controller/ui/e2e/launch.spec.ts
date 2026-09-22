import AxeBuilder from '@axe-core/playwright';
import { expect, test, type Page, type Route } from '@playwright/test';
import { PLAYBOOK_RUN, stubApi } from './api';

const LAUNCH = '/playbooks/survey/launch';

async function ready(page: Page, path: string): Promise<void> {
  const crashes: string[] = [];
  page.on('pageerror', (e) => crashes.push(String(e)));
  await page.goto(path);
  await page.waitForLoadState('networkidle');
  expect(crashes, `uncaught exceptions on ${path}:\n${crashes.join('\n')}`).toEqual([]);
  await expect(page.locator('main')).toBeVisible();
}

/// Refuse the launch the way the endpoint does, naming the field that failed.
async function refuseWith(page: Page, field: string, message: string): Promise<void> {
  await page.route('**/api/playbooks/survey/launch', (route: Route) =>
    route.fulfill({
      status: 422,
      contentType: 'application/json',
      body: JSON.stringify({
        error: "the supplied parameters do not satisfy the playbook's schema",
        fields: [{ field, message }],
      }),
    }),
  );
}

test.describe('playbook launch form', () => {
  test('renders the pack schema with defaults and doc strings', async ({ page }) => {
    await stubApi(page);
    await ready(page, LAUNCH);

    await expect(page.locator('#launch-param-topic')).toHaveValue('');
    await expect(page.locator('#launch-param-depth')).toHaveValue('shallow');
    await expect(page.getByText('What the survey covers.')).toBeVisible();
    await expect(page.getByText('How far to chase citations.')).toBeVisible();
  });

  test('checks the pattern on blur', async ({ page }) => {
    await stubApi(page);
    await ready(page, LAUNCH);

    await page.locator('#launch-param-topic').fill('ATTENTION');
    await page.locator('#launch-param-topic').blur();

    await expect(page.getByRole('alert')).toHaveText('topic must match ^[a-z ]+$');
    await expect(page.locator('#launch-param-topic')).toHaveAttribute('aria-invalid', 'true');
  });

  test('ceilings are their own section, bounded by the deploy caps', async ({ page }) => {
    await stubApi(page);
    await ready(page, LAUNCH);

    await expect(page.getByText('Launcher-owned')).toBeVisible();
    await expect(page.getByText('Capped at $25 per run.')).toBeVisible();
    await expect(page.locator('#launch-max-time')).toHaveValue('30m');

    await page.locator('#launch-max-time').fill('6h');
    await page.locator('#launch-max-time').blur();
    await expect(
      page.getByRole('alert').filter({ hasText: "max_time is above this controller's cap of 4h" }),
    ).toBeVisible();
  });

  test('a server rejection lands on the field that produced it', async ({ page }) => {
    await stubApi(page);
    await refuseWith(page, 'topic', '"attention" is already covered by a live run');
    await ready(page, LAUNCH);

    await page.locator('#launch-param-topic').fill('attention');
    await page.getByRole('button', { name: 'LAUNCH' }).click();

    await expect(
      page.getByRole('alert').filter({ hasText: 'is already covered by a live run' }),
    ).toBeVisible();
    await expect(page.locator('#launch-param-topic')).toHaveAttribute('aria-invalid', 'true');
  });

  test('a schedule is withheld until its firings are previewed', async ({ page }) => {
    await stubApi(page);
    await ready(page, LAUNCH);

    await page.getByRole('button', { name: 'ON A SCHEDULE' }).click();
    await expect(page.locator('#launch-cron-expr')).toHaveValue('0 6 * * MON-FRI');
    await expect(page.getByRole('button', { name: 'SCHEDULE', exact: true })).toBeDisabled();

    await page.locator('#launch-tz').fill('UTC');
    await page.getByRole('button', { name: 'PREVIEW FIRINGS' }).click();

    await expect(page.getByText('2026-08-24T06:00:00Z')).toBeVisible();
    await expect(page.getByRole('button', { name: 'SCHEDULE', exact: true })).toBeEnabled();

    await page.locator('#launch-cron-expr').fill('0 7 * * *');
    await expect(page.getByText('2026-08-24T06:00:00Z')).toBeHidden();
    await expect(page.getByRole('button', { name: 'SCHEDULE', exact: true })).toBeDisabled();
  });

  test('a relaunch prefills the form from the run snapshot, still editable', async ({ page }) => {
    await stubApi(page);
    await ready(page, `${LAUNCH}?relaunch=playbook%3Asurvey%3A0199`);

    await expect(page.locator('#launch-param-topic')).toHaveValue('attention kernels');
    await expect(page.locator('#launch-param-depth')).toHaveValue('deep');
    await expect(page.locator('#launch-max-time')).toHaveValue('2h');
    await expect(page.getByText('Prefilled from playbook:survey:0199')).toBeVisible();

    await page.locator('#launch-param-topic').fill('paged attention');
    await expect(page.locator('#launch-param-topic')).toHaveValue('paged attention');
  });

  test('the runs rail relaunches a listed run', async ({ page }) => {
    await stubApi(page);
    await page.route('**/api/playbook-runs', (route: Route) =>
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify([PLAYBOOK_RUN]),
      }),
    );
    await ready(page, '/playbook-runs');

    await page.getByRole('link', { name: 'RELAUNCH' }).click();
    await expect(page.locator('#launch-param-topic')).toHaveValue('attention kernels');
  });

  /// The launch form says what substrate the pack needs and where this deployment will run it,
  /// so the ceiling section is not the only thing bounding the launch a reader can see.
  test('the form names the substrate the run will use', async ({ page }) => {
    await stubApi(page);
    await ready(page, LAUNCH);

    await expect(page.getByText('Substrate')).toBeVisible();
    await expect(page.getByText('[agent] backend = local')).toBeVisible();
    await expect(page.getByText('local dispatch')).toBeVisible();
    await expect(page.getByText('Cannot dispatch')).toHaveCount(0);
  });

  test('the registry and the form are accessible', async ({ page }) => {
    await stubApi(page);
    for (const path of ['/playbooks', LAUNCH]) {
      await ready(page, path);
      const { violations } = await new AxeBuilder({ page })
        .withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'])
        .analyze();
      const summary = violations.map((v) => `${v.id} (${v.nodes.length}): ${v.help}`).join('\n');
      expect(violations, `${path}\n${summary}`).toEqual([]);
    }
  });
});
