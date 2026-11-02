import { expect, test } from '@playwright/test';
import { stubApi } from './api';

test.describe('a registered playbook', () => {
  test('shows its source in a pane tall enough to read', async ({ page }) => {
    await stubApi(page);
    await page.goto('/playbooks/survey');
    const source = page.getByTestId('playbook-inspector');
    await expect(source.locator('.monaco-editor')).toBeVisible();
    await expect(source).toContainText('[repo]');
    const height = (await source.boundingBox())?.height ?? 0;
    expect(height).toBeGreaterThan(400);
  });
});
