import AxeBuilder from '@axe-core/playwright';
import { expect, test, type Page } from '@playwright/test';
import { stubApi } from './api';

const LAUNCH = '/playbook-runs/playbook%3Asurvey%3A0199';
const FAILED = '/playbook-runs/playbook%3Asurvey%3A0198';
const SECRETS_REFUSED = '/playbook-runs/playbook%3Asurvey%3A0197';

async function ready(page: Page, path: string): Promise<void> {
  const crashes: string[] = [];
  page.on('pageerror', (e) => crashes.push(String(e)));
  await page.goto(path);
  await page.waitForLoadState('networkidle');
  expect(crashes, `uncaught exceptions on ${path}:\n${crashes.join('\n')}`).toEqual([]);
  await expect(page.locator('main')).toBeVisible();
}

test.describe('the launch view', () => {
  /// A launch is authorized, then dispatched: the values and ceilings it froze, where it came
  /// from, and what its run did — none of which the issue journey has a place for.
  test('shows the frozen params, the ceilings and the origin link', async ({ page }) => {
    await stubApi(page);
    await ready(page, LAUNCH);

    const params = page.getByTestId('launch-params');
    await expect(params).toContainText('topic');
    await expect(params).toContainText('attention kernels');
    await expect(page.getByText('max $12.00 / 2h')).toBeVisible();
    await expect(page.getByRole('link', { name: 'survey' })).toHaveAttribute(
      'href',
      '/playbooks/survey/launch',
    );
    await expect(page.getByRole('link', { name: 'RELAUNCH' })).toBeVisible();
  });

  /// Dispatch is a field of its own with the failure where a person looks for it, not a line of
  /// ranking rationale on a page of blanks.
  test('makes a failed dispatch first-class, with its reason', async ({ page }) => {
    await stubApi(page);
    await ready(page, FAILED);

    await expect(page.getByTestId('dispatch-state')).toContainText('failed');
    await expect(page.getByTestId('dispatch-failure')).toContainText(
      'needs a deploy profile',
    );
    await expect(page.getByText('3 failed')).toBeVisible();
    await expect(page.getByText('Ranking rationale')).toHaveCount(0);
    await expect(page.getByText('Journey')).toHaveCount(0);
  });

  /// A launch that resolved no credentials says which one, and offers the page that fixes it —
  /// the refusal is the whole of what the launcher can act on.
  test('names the secret a refused launch has no binding for', async ({ page }) => {
    await stubApi(page);
    await ready(page, SECRETS_REFUSED);

    await expect(page.getByTestId('secrets-refusal')).toContainText('pr_token');
    await expect(page.getByTestId('secrets-refusal')).toContainText('neuralmagic/crucible');
    await expect(page.getByRole('link', { name: 'OPEN SECRETS' })).toHaveAttribute(
      'href',
      '/secrets',
    );
    // The generic parked block would only repeat the same text with the prefix on it.
    await expect(page.getByText('Parked', { exact: true })).toHaveCount(0);
  });

  /// A dispatched launch draws its run's task graph on the same surface the wizard and the studio
  /// use, per-instance status included.
  test('draws the dispatched run task graph', async ({ page }) => {
    await stubApi(page);
    await ready(page, LAUNCH);

    const graph = page.getByTestId('workflow-graph');
    await expect(graph).toBeVisible();
    await expect(graph.locator('[data-task="read"]')).toContainText('pass');
    await expect(graph.locator('[data-task="summarize[flashinfer]"]')).toContainText('fail');
    await expect(page.getByRole('link', { name: 'RUN-0412' })).toBeVisible();
  });

  /// The graph runs left to right until this device says otherwise; turning it top down stacks
  /// the layers, and the choice outlives the page.
  test('turns the task graph top down and remembers it', async ({ page }) => {
    await stubApi(page);
    await ready(page, LAUNCH);

    const graph = page.getByTestId('workflow-graph');
    const read = graph.locator('[data-task="read"]');
    const summarize = graph.locator('[data-task="summarize"]');
    const box = async (card: typeof read) => {
      const found = await card.boundingBox();
      if (found === null) throw new Error('card is not laid out');
      return found;
    };

    const toggle = graph.getByRole('toolbar', { name: 'Graph view' });
    await expect(toggle.getByRole('button', { name: 'Lay the graph out left to right' })).toHaveAttribute('aria-pressed', 'true');
    let a = await box(read);
    let b = await box(summarize);
    expect(b.x).toBeGreaterThan(a.x + a.width);
    expect(Math.abs(b.y - a.y)).toBeLessThan(a.height);

    await toggle.getByRole('button', { name: 'Lay the graph out top down' }).click();
    await expect(toggle.getByRole('button', { name: 'Lay the graph out top down' })).toHaveAttribute('aria-pressed', 'true');
    await expect.poll(async () => (await box(summarize)).y - (await box(read)).y).toBeGreaterThan(0);
    a = await box(read);
    b = await box(summarize);
    expect(Math.abs(b.x - a.x)).toBeLessThan(2);
    expect(b.y).toBeGreaterThan(a.y + a.height);

    await page.reload();
    await expect(graph).toBeVisible();
    await expect(
      graph.getByRole('toolbar', { name: 'Graph view' }).getByRole('button', { name: 'Lay the graph out top down' }),
    ).toHaveAttribute('aria-pressed', 'true');
    a = await box(read);
    b = await box(summarize);
    expect(b.y).toBeGreaterThan(a.y + a.height);
  });

  /// A launch key opens the launch view wherever it renders, and an old issue link redirects to it
  /// rather than rendering a ladder a playbook never climbs.
  test('an issue link to a launch redirects here', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/issues/playbook%3Asurvey%3A0199');

    await expect(page).toHaveURL(/\/playbook-runs\/playbook%3Asurvey%3A0199$/);
    await expect(page.getByTestId('launch-params')).toBeVisible();
  });

  test('the launch view is accessible', async ({ page }) => {
    await stubApi(page);
    for (const path of [LAUNCH, FAILED]) {
      await ready(page, path);
      const { violations } = await new AxeBuilder({ page })
        .withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'])
        .analyze();
      const summary = violations.map((v) => `${v.id} (${v.nodes.length}): ${v.help}`).join('\n');
      expect(violations, `${path}\n${summary}`).toEqual([]);
    }
  });
});
