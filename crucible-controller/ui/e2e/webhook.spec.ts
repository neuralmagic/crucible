import { expect, test } from '@playwright/test';
import { stubApi } from './api';

test.describe('the webhook form', () => {
  test('keeps what the author types before the editor has loaded', async ({ page }) => {
    await stubApi(page);
    let release: () => void = () => undefined;
    const loaded = new Promise<void>((resolve) => {
      release = resolve;
    });
    await page.route('**/src/editor/CodeSurfaceImpl.tsx*', async (route) => {
      await loaded;
      await route.continue();
    });
    const checked: string[] = [];
    page.on('request', (request) => {
      if (request.url().endsWith('/api/webhooks/check')) checked.push(request.postData() ?? '');
    });

    await page.goto('/playbooks/survey/webhook');
    const fallback = page.getByTestId('cel-filter-loading');
    await expect(fallback).toBeVisible();
    await fallback.fill('body.action == "opened"');

    release();
    await expect(page.getByTestId('cel-filter').locator('.monaco-editor')).toBeVisible();
    await expect(page.getByTestId('cel-filter')).toContainText('body.action');
    await expect.poll(() => checked.some((b) => b.includes('body.action == \\"opened\\"'))).toBe(true);
  });
});
