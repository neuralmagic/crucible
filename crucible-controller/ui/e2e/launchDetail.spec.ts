import AxeBuilder from '@axe-core/playwright';
import { expect, test, type Page, type Route } from '@playwright/test';
import { ROUTES, stubApi } from './api';

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


/// Launch 0199 with its run finished, which is when it offers a rerun.
async function finished(page: Page): Promise<void> {
  const detail = ROUTES['/api/playbook-runs/playbook:survey:0199'];
  if (typeof detail !== 'object' || detail === null || Array.isArray(detail) || !Array.isArray(detail.runs)) {
    throw new Error('launch 0199 fixture has no runs');
  }
  const runs = detail.runs.map((run) =>
    typeof run === 'object' && run !== null && !Array.isArray(run) ? { ...run, status: 'failed' } : run,
  );
  const body = { ...detail, runs };
  await page.route('**/api/playbook-runs/playbook%3Asurvey%3A0199', (route: Route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(body) }),
  );
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
    await expect(page.getByRole('link', { name: 'survey', exact: true })).toHaveAttribute(
      'href',
      '/playbooks/survey/launch',
    );
    await expect(page.getByRole('link', { name: 'Relaunch' })).toBeVisible();
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
    await expect(page.getByRole('link', { name: 'Open secrets' })).toHaveAttribute(
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

  /// A finished run offers a rerun: what it would reuse at what cost, what runs again, and a way
  /// to widen it from a task. The rerun is a launch of the same pack with the same values.
  test('reruns a finished run from a chosen task', async ({ page }) => {
    await stubApi(page);
    await finished(page);
    const asked: string[] = [];
    await page.route('**/api/runs/RUN-0412/rerun**', (route: Route) => {
      const from = new URL(route.request().url()).searchParams.get('from') ?? '';
      asked.push(from);
      const widened = from.includes('summarize');
      return route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          run_id: 'RUN-0412',
          playbook: 'survey',
          draft_version: null,
          reused: widened
            ? [{ task: 'discover', cost_usd: 0.25 }]
            : [
                { task: 'discover', cost_usd: 0.25 },
                { task: 'summarize', cost_usd: 1.5 },
              ],
          rerun: widened ? ['rank', 'summarize'] : ['rank'],
          reused_usd: widened ? 0.25 : 1.75,
          changed: [],
        }),
      });
    });
    const posted: unknown[] = [];
    await page.route('**/api/playbooks/survey/launch', (route: Route) => {
      posted.push(JSON.parse(route.request().postData() ?? 'null'));
      return route.fulfill({
        status: 201,
        contentType: 'application/json',
        body: JSON.stringify({ key: 'playbook:survey:0300' }),
      });
    });
    await ready(page, LAUNCH);

    const tasks = page.getByTestId('rerun-tasks');
    await expect(tasks.locator('li[data-state="reused"]')).toHaveCount(2);
    await expect(tasks.locator('li[data-state="reruns"]')).toContainText('rank');
    await expect(page.getByText('$1.75 reused')).toBeVisible();

    await tasks.locator('li', { hasText: 'summarize' }).getByRole('button', { name: 'Rerun from' }).click();
    await expect(page.getByRole('button', { name: 'Rerun 2' })).toBeVisible();
    expect(asked).toContain('summarize');

    await page.getByRole('button', { name: 'Rerun 2' }).click();
    await expect(page).toHaveURL(/\/playbook-runs\/playbook%3Asurvey%3A0300$/);
    expect(posted).toEqual([
      {
        params: { topic: 'attention kernels', depth: 'deep' },
        max_cost: 12,
        max_time: '2h',
        rerun: { of: 'RUN-0412', from: ['summarize'] },
      },
    ]);
  });

  /// An edit to a task the rerun would reuse is named before anything launches, with the one move
  /// that makes the rerun honest: run that task again too.
  test('holds a rerun whose reused task changed until it is included', async ({ page }) => {
    await stubApi(page);
    await finished(page);
    await page.route('**/api/runs/RUN-0412/rerun**', (route: Route) => {
      const from = new URL(route.request().url()).searchParams.get('from') ?? '';
      const included = from.includes('discover');
      return route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          run_id: 'RUN-0412',
          playbook: 'survey',
          draft_version: null,
          reused: [],
          rerun: included ? ['discover', 'rank', 'summarize'] : [],
          reused_usd: 0,
          changed: included ? [] : ['discover'],
        }),
      });
    });
    await ready(page, LAUNCH);

    const tasks = page.getByTestId('rerun-tasks');
    await expect(tasks.locator('li[data-state="changed"]')).toContainText('discover');
    await expect(page.getByRole('button', { name: /^Rerun \d/ })).toBeDisabled();
    await tasks.getByRole('button', { name: 'Include' }).click();
    await expect(page.getByRole('button', { name: 'Rerun 3' })).toBeEnabled();
  });
});
