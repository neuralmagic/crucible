import { expect, test, type Page } from '@playwright/test';
import { stubApi } from './api';

const EVENTS = [
  {
    ts: '2026-08-24T11:58:00Z',
    key: 'draft:studio',
    from: 'generated',
    to: 'generated',
    reason: {
      text: 'draft studio generated from an intent: 1 rounds, $1.4679, engine check passed',
      truncated: false,
    },
    actor: 'wren',
    evidence: null,
  },
  {
    ts: '2026-08-24T11:52:00Z',
    key: 'playbook:survey:0199',
    from: 'created',
    to: 'created',
    reason: { text: 'launch created from the draft studio', truncated: false },
    actor: 'wren',
    evidence: null,
  },
  {
    ts: '2026-08-24T11:40:00Z',
    key: 'owner/repo#8',
    from: 'new',
    to: 'scoped',
    reason: { text: 'no repro', truncated: false },
    actor: 'wren',
    evidence: null,
  },
  {
    ts: '2026-08-24T11:20:00Z',
    key: 'schedule:nightly',
    from: 'enabled',
    to: 'disabled',
    reason: { text: 'quiet hours', truncated: false },
    actor: 'wren',
    evidence: null,
  },
];

async function ready(page: Page): Promise<void> {
  const crashes: string[] = [];
  page.on('pageerror', (e) => crashes.push(String(e)));
  await stubApi(page);
  await page.route('**/api/events', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(EVENTS) }),
  );
  await page.goto('/activity');
  await page.waitForLoadState('networkidle');
  expect(crashes).toEqual([]);
  await expect(page.locator('main')).toBeVisible();
}

const row = (page: Page, key: string) => page.locator('tr').filter({ has: page.locator(`[title="${key}"]`) });

test.describe('the activity feed', () => {
  test('an occurrence event renders one chip, not a self-pointing arrow', async ({ page }) => {
    await ready(page);

    const draft = row(page, 'draft:studio');
    await expect(draft).toBeVisible();
    await expect(draft.locator('[data-ui="status"]')).toHaveCount(1);
    await expect(draft.getByText('→')).toHaveCount(0);
  });

  test('a status transition keeps its arrow', async ({ page }) => {
    await ready(page);

    const issue = row(page, 'owner/repo#8');
    await expect(issue.locator('[data-ui="status"]')).toHaveCount(2);
    await expect(issue.getByText('→')).toBeVisible();
  });

  test('event keys link to their own surfaces', async ({ page }) => {
    await ready(page);

    await expect(row(page, 'draft:studio').locator('a[title="draft:studio"]')).toHaveAttribute(
      'href',
      '/playbooks/drafts/studio',
    );
    await expect(
      row(page, 'playbook:survey:0199').locator('a[title="playbook:survey:0199"]'),
    ).toHaveAttribute('href', '/playbook-runs/playbook%3Asurvey%3A0199');
    await expect(row(page, 'owner/repo#8').locator('a[title="owner/repo#8"]')).toHaveAttribute(
      'href',
      '/issues/owner%2Frepo%238',
    );
  });

  test('a key with no page of its own stays text', async ({ page }) => {
    await ready(page);

    const schedule = row(page, 'schedule:nightly');
    await expect(schedule.locator('a')).toHaveCount(0);
    await expect(schedule.locator('[title="schedule:nightly"]')).toBeVisible();
  });
});
