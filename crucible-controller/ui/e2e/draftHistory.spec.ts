import { expect, test, type Page } from '@playwright/test';
import { stubApi } from './api';

const HISTORY_PAGE = '/playbooks/drafts/studio/history';

function save(version: number, previous: number | null, changes: unknown, compiles = true) {
  return {
    version,
    previous_version: previous,
    tree_digest: `tree1:${String(version).repeat(64)}`,
    schema_digest: compiles ? `sha256:${version}` : null,
    diagnostics: compiles ? 0 : 2,
    core_rev: '7c2c1a5',
    created_by: version % 2 === 0 ? 'agent:author' : 'wren',
    created_at: `2026-08-22T09:0${version}:00Z`,
    changes,
  };
}

const HISTORY = [
  save(4, 3, [{ path: 'workflow.star', change: 'modified' }], false),
  save(3, 2, []),
  save(2, 1, [
    { path: 'skills/review.md', change: 'added' },
    { path: 'workflow.star', change: 'modified' },
  ]),
  save(1, null, [
    { path: 'crucible.toml', change: 'added' },
    { path: 'workflow.star', change: 'added' },
  ]),
];

function diffOf(from: number | null, to: number) {
  return {
    from,
    to,
    files:
      from === to
        ? []
        : [
            {
              path: 'workflow.star',
              change: 'modified',
              additions: 1,
              deletions: 1,
              unified: `--- a/workflow.star\n+++ b/workflow.star\n@@ -1,2 +1,2 @@\n params = {}\n-save ${from ?? 0}\n+save ${to}\n`,
            },
          ],
  };
}

/// Serves the history and answers each diff request with the range it was asked for, recording
/// the requests so a test can tell what the page fetched.
async function stubHistory(page: Page): Promise<string[]> {
  const asked: string[] = [];
  await stubApi(page);
  await page.route('**/api/playbook-drafts/studio/history', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(HISTORY) }),
  );
  await page.route('**/api/playbook-drafts/studio/diff*', (route) => {
    const query = new URL(route.request().url()).searchParams;
    asked.push(query.toString());
    const to = Number(query.get('to') ?? 4);
    const from = query.get('from');
    const before = HISTORY.find((entry) => entry.version === to)?.previous_version ?? null;
    return route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(diffOf(from === null ? before : Number(from), to)),
    });
  });
  return asked;
}

test('the history opens on what the newest save changed', async ({ page }) => {
  await stubHistory(page);
  await page.goto(HISTORY_PAGE);

  await expect(page.getByTestId('draft-history-saves').locator('> li')).toHaveCount(4);
  await expect(page.getByTestId('draft-history-range')).toHaveText('v3 → v4');
  await expect(page.getByTestId('draft-history-save-4')).toHaveAttribute('data-placement', 'to');
  await expect(page.getByTestId('draft-history-save-3')).toHaveAttribute('data-placement', 'from');
  await expect(page.getByTestId('draft-history-save-4')).toContainText('2 diagnostics');
  await expect(page.getByTestId('draft-history-save-3')).toContainText('No file changes');
  await expect(page.getByTestId('draft-history-save-2')).toContainText('skills/review.md');

  const file = page.getByTestId('draft-history-file-workflow.star');
  await expect(file.locator('[data-kind="del"]')).toHaveText('2-save 3');
  await expect(file.locator('[data-kind="add"]')).toHaveText('2+save 4');
  await expect(page.getByTestId('draft-history-bisect')).toHaveCount(0);
  await expect(page.getByTestId('draft-history-newer')).toBeDisabled();
});

test('a wide range halves down to the one save that made the change', async ({ page }) => {
  const asked = await stubHistory(page);
  await page.goto(HISTORY_PAGE);

  await page.getByTestId('draft-history-save-1').getByRole('link', { name: 'From' }).click();
  await expect(page.getByTestId('draft-history-range')).toHaveText('v1 → v4');
  await expect(page).toHaveURL(/\?from=1&to=4$/);
  await expect(page.getByTestId('draft-history-save-2')).toHaveAttribute('data-placement', 'inside');
  await expect(page.getByTestId('draft-history-bisect')).toContainText('spans 3 saves');

  await page.getByTestId('draft-history-newer-half').click();
  await expect(page.getByTestId('draft-history-range')).toHaveText('v3 → v4');
  await expect(page.getByTestId('draft-history-bisect')).toHaveCount(0);

  await page.goto(`${HISTORY_PAGE}?from=1&to=4`);
  await page.getByTestId('draft-history-older-half').click();
  await expect(page.getByTestId('draft-history-range')).toHaveText('v1 → v3');
  await page.getByTestId('draft-history-older-half').click();
  await expect(page.getByTestId('draft-history-range')).toHaveText('v1 → v2');
  await expect(page.getByTestId('draft-history-bisect')).toHaveCount(0);
  expect(asked).toContain('from=1&to=2');
});

test('the history walks one save at a time and links back to the studio', async ({ page }) => {
  await stubHistory(page);
  await page.goto(`${HISTORY_PAGE}?to=2`);

  await expect(page.getByTestId('draft-history-range')).toHaveText('v1 → v2');
  await page.getByTestId('draft-history-older').click();
  await expect(page.getByTestId('draft-history-range')).toHaveText('empty → v1');
  await expect(page.getByTestId('draft-history-older')).toBeDisabled();
  await page.getByTestId('draft-history-newer').click();
  await page.getByTestId('draft-history-newer').click();
  await expect(page.getByTestId('draft-history-range')).toHaveText('v2 → v3');

  await page.getByRole('link', { name: 'Open studio' }).click();
  await expect(page).toHaveURL(/\/playbooks\/drafts\/studio$/);
  await page.getByTestId('draft-history').click();
  await expect(page).toHaveURL(/\/playbooks\/drafts\/studio\/history$/);
});
