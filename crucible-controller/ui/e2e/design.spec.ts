import AxeBuilder from '@axe-core/playwright';
import { expect, test, type Page } from '@playwright/test';
import { stubApi } from './api';

const ROUTES = [
  { path: '/', name: 'dashboard' },
  { path: '/issues', name: 'issues' },
  { path: '/runs', name: 'runs' },
  { path: '/inbox', name: 'inbox' },
  { path: '/approvals', name: 'approvals' },
  { path: '/repos', name: 'repos' },
  { path: '/schedules', name: 'schedules' },
  { path: '/activity', name: 'activity' },
  { path: '/secrets', name: 'secrets' },
];

const THEMES = ['light', 'dark'] as const;

async function settle(page: Page, theme: string): Promise<void> {
  await page.emulateMedia({ colorScheme: theme === 'dark' ? 'dark' : 'light' });
  await page.addInitScript((t: string) => {
    localStorage.setItem('theme', t);
  }, theme);
}

async function ready(page: Page, path: string): Promise<void> {
  const crashes: string[] = [];
  page.on('pageerror', (e) => crashes.push(String(e)));

  await page.goto(path);
  await page.waitForLoadState('networkidle');
  await page.evaluate(() => document.fonts.ready);

  expect(crashes, `uncaught exceptions on ${path}:\n${crashes.join('\n')}`).toEqual([]);
  await expect(page.locator('main')).toBeVisible();
  await expect(page.locator('#root')).not.toBeEmpty();
}

for (const theme of THEMES) {
  test.describe(`${theme} theme`, () => {
    for (const route of ROUTES) {
      test(`${route.name} matches its baseline`, async ({ page }) => {
        await stubApi(page);
        await settle(page, theme);
        await ready(page, route.path);

        await expect(page).toHaveScreenshot(`${route.name}-${theme}.png`, { fullPage: true });
      });
    }
  });
}

test.describe('issue facets', () => {
  test('the repo axis caps its options and expands on demand', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/issues');

    const repo = page
      .locator('dl[aria-label="Filters"] > div')
      .filter({ has: page.getByText('Repo', { exact: true }) });
    const options = repo.locator('dd button');

    // Eight repos in the fixture, capped to All + 6 with a +2 toggle.
    await expect(options.filter({ hasText: '+2' })).toHaveCount(1);
    const capped = await options.count();

    await options.filter({ hasText: '+2' }).click();
    expect(await options.count()).toBeGreaterThan(capped);
    await expect(options.filter({ hasText: 'less' })).toHaveCount(1);
  });

  test('options with no results are present but not clickable', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/issues');

    const zero = page.locator('dl[aria-label="Filters"] dd button', { hasText: /^building0$/ });
    await expect(zero).toHaveCount(1);
    await expect(zero).toBeDisabled();
  });
});

test.describe('design rules', () => {
  test('nothing in the product is rounded and nothing casts a shadow', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/issues');

    const violations = await page.evaluate(() => {
      const bad: string[] = [];
      for (const el of document.querySelectorAll('*')) {
        const s = getComputedStyle(el);
        const radius = [s.borderTopLeftRadius, s.borderTopRightRadius, s.borderBottomLeftRadius, s.borderBottomRightRadius];
        const rounded = radius.some((r) => r !== '0px' && r !== '');
        const shadowed = s.boxShadow !== 'none' && s.boxShadow !== '';
        if (rounded || shadowed) {
          bad.push(`${el.tagName.toLowerCase()}.${el.className.toString().slice(0, 40)} radius=${radius[0]} shadow=${s.boxShadow}`);
        }
      }
      return bad;
    });

    expect(violations, `rounded or shadowed elements:\n${violations.join('\n')}`).toEqual([]);
  });

  test('numeric table cells carry tabular figures', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/runs');

    const cells = await page.evaluate(() =>
      [...document.querySelectorAll('td')]
        .filter((td) => /^[$\d]/.test(td.textContent?.trim() ?? ''))
        .map((td) => getComputedStyle(td).fontVariantNumeric),
    );

    expect(cells.length).toBeGreaterThan(0);
    for (const v of cells) expect(v).toContain('tabular-nums');
  });
});

for (const theme of THEMES) {
  test.describe(`${theme} accessibility`, () => {
    for (const route of ROUTES) {
      test(`${route.name} has no WCAG A/AA violations`, async ({ page }) => {
        await stubApi(page);
        await settle(page, theme);
        await ready(page, route.path);

        const { violations } = await new AxeBuilder({ page })
          .withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'])
          .analyze();

        const summary = violations.map((v) => `${v.id} (${v.nodes.length}): ${v.help}`).join('\n');
        expect(violations, summary).toEqual([]);
      });
    }
  });
}
