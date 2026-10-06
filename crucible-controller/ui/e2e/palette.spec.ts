import { expect, test, type Page } from '@playwright/test';
import { stubApi } from './api';

const DOCS = 'https://neuralmagic.github.io/crucible/';

/// A one-heading book, written the way mdBook writes `searchindex.js`.
async function stubDocs(page: Page): Promise<void> {
  const index = {
    doc_urls: ['playbooks.html#your-first-playbook'],
    index: {
      documentStore: {
        docs: {
          '0': {
            id: '0',
            title: 'Your first playbook',
            breadcrumbs: 'Playbooks » Your first playbook',
            body: '',
          },
        },
      },
    },
  };
  await page.context().route(`${DOCS}**`, (route) =>
    route.request().url().endsWith('searchindex.js')
      ? route.fulfill({
          status: 200,
          contentType: 'application/javascript',
          body: `window.search = Object.assign(window.search, JSON.parse('${JSON.stringify(index)}'));`,
        })
      : route.fulfill({ status: 200, contentType: 'text/html', body: '<title>docs</title>' })
  );
}

const palette = (page: Page) => page.getByRole('dialog', { name: 'Command palette' });
const search = (page: Page) => palette(page).getByRole('combobox');

async function open(page: Page): Promise<void> {
  await stubApi(page);
  await stubDocs(page);
  await page.goto('/');
  await page.waitForLoadState('networkidle');
  await page.keyboard.press('ControlOrMeta+k');
  await expect(palette(page)).toBeVisible();
}

test.describe('the command palette', () => {
  test('opens on Cmd/Ctrl+K with every group in order', async ({ page }) => {
    await open(page);

    await expect(palette(page).locator('[cmdk-group-heading]')).toHaveText([
      'Pages',
      'Playbooks',
      'Drafts',
      'Recent runs',
      'Docs',
    ]);
    await expect(palette(page).getByRole('option', { name: /^Edit studio/ })).toBeVisible();
    await expect(palette(page).getByRole('option', { name: /^Launch survey/ })).toBeVisible();
  });

  test('closes on Escape and toggles on the shortcut', async ({ page }) => {
    await open(page);

    await page.keyboard.press('Escape');
    await expect(palette(page)).toBeHidden();
    await page.keyboard.press('ControlOrMeta+k');
    await expect(palette(page)).toBeVisible();
    await page.keyboard.press('ControlOrMeta+k');
    await expect(palette(page)).toBeHidden();
  });

  test('ranks a title match first and opens it on Enter', async ({ page }) => {
    await open(page);

    await search(page).fill('edit');
    await expect(palette(page).getByRole('option').first()).toHaveText(/^Edit studio/);
    await page.keyboard.press('Enter');

    await expect(page).toHaveURL('/playbooks/drafts/studio');
    await expect(palette(page)).toBeHidden();
  });

  test('launches a playbook and opens a run', async ({ page }) => {
    await open(page);
    await search(page).fill('launch survey');
    await page.keyboard.press('Enter');
    await expect(page).toHaveURL('/playbooks/survey/launch');

    await page.keyboard.press('ControlOrMeta+k');
    await search(page).fill('triage running');
    await palette(page).getByRole('option', { name: /^triage/ }).first().click();
    await expect(page).toHaveURL(`/playbook-runs/${encodeURIComponent('playbook:triage:0201')}`);
  });

  test('opens a docs heading in a new tab', async ({ page }) => {
    await open(page);
    await search(page).fill('first playbook');
    const tab = page.context().waitForEvent('page');
    await palette(page).getByRole('option', { name: /^Your first playbook/ }).click();

    await expect(await tab).toHaveURL(`${DOCS}playbooks.html#your-first-playbook`);
  });

  test('shows a failed list in its own group and keeps the rest', async ({ page }) => {
    await stubApi(page);
    await stubDocs(page);
    await page.route('**/api/playbooks', (route) =>
      route.fulfill({ status: 500, contentType: 'application/json', body: '{"error":"boom"}' })
    );
    await page.goto('/');
    await page.waitForLoadState('networkidle');
    await page.keyboard.press('ControlOrMeta+k');

    await expect(palette(page).getByRole('option', { name: /^Couldn't load/ })).toBeVisible();
    await expect(palette(page).getByRole('option', { name: /^Edit studio/ })).toBeVisible();
    await expect(palette(page).getByRole('option', { name: /^Launch survey/ })).toHaveCount(0);
  });

  test('leaves a Cmd/Ctrl+K another handler took alone', async ({ page }) => {
    await stubApi(page);
    await stubDocs(page);
    await page.goto('/');
    await page.waitForLoadState('networkidle');
    await page.evaluate(() => {
      document.body.addEventListener('keydown', (event) => {
        event.preventDefault();
      });
    });
    await page.keyboard.press('ControlOrMeta+k');

    await expect(palette(page)).toHaveCount(0);
  });
});
