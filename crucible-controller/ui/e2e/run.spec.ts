import { expect, test, type Page } from '@playwright/test';
import { stubApi, TRIAGE_RUN } from './api';

const RUN = '/runs/RUN-0412';

async function ready(page: Page, path: string): Promise<void> {
  const crashes: string[] = [];
  page.on('pageerror', (e) => crashes.push(String(e)));
  await page.goto(path);
  await page.waitForLoadState('networkidle');
  expect(crashes, `uncaught exceptions on ${path}:\n${crashes.join('\n')}`).toEqual([]);
  await expect(page.locator('main')).toBeVisible();
}

test.describe('the run task graph', () => {
  /// The graph is the run's own, not a compiled plan's: every card says how its task ended, a
  /// fanned-out instance the plan never declared is drawn too, and the failed one reads as failed.
  test('draws the admitted plan with each task run state', async ({ page }) => {
    await stubApi(page);
    await ready(page, RUN);

    const graph = page.getByTestId('workflow-graph');
    await expect(graph).toBeVisible();
    // Instances hang off their mapped task and become the producers for its downstream edge.
    await expect(graph.locator('.react-flow__edges path.react-flow__edge-path')).toHaveCount(6);

    const read = graph.locator('[data-task="read"]');
    await expect(read).toContainText('pass');
    await expect(read).toContainText('⟳ survey');
    // Two attempts, summed cost and duration.
    await expect(read).toContainText('iter 1 · 2 attempts · $1.50 · 4m 31s');
    await expect(graph.locator('[data-task="file"]')).not.toContainText('iter');

    const failed = graph.locator('[data-task="summarize[flashinfer]"]');
    await expect(failed).toContainText('fail');
    await expect(graph.locator('[data-task="summarize[paged-attention]"]')).toContainText('pass');
    await expect(page.getByText('red = failed')).toBeVisible();
  });

  /// Clicking a task opens what only a run knows about it, its result payload included.
  test('opens a failed instance result in the metadata panel', async ({ page }) => {
    await stubApi(page);
    await ready(page, RUN);

    const graph = page.getByTestId('workflow-graph');
    await graph.locator('[data-task="summarize[flashinfer]"]').click();

    const panel = page.getByRole('complementary', { name: 'Task summarize[flashinfer]' });
    await expect(panel).toBeVisible();
    await expect(panel).toContainText('no citations parsed');
    await expect(panel.getByTestId('task-result')).toContainText('at summarize.sh:14');
    const value = (label: string) =>
      panel.locator('dt', { hasText: new RegExp(`^${label}$`) }).locator('+ dd');
    await expect(value('status')).toHaveText('fail');
    await expect(value('attempts')).toHaveText('1');
    await expect(value('cost')).toHaveText('$0.05');
    await expect(value('took')).toHaveText('6s');
  });

  /// The evidence a finished run left behind reaches the panel: what the task emitted, and the
  /// file it captured, picked out of the list and read beside it rather than by digging through the
  /// run directory.
  /// A file a failed task captured is its failure's evidence, and the listing says so on exactly
  /// that file: the passing sibling's and the reducer's carry no such mark.
  test('marks a captured file whose task failed as failure evidence', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/runs/RUN-0412');

    const list = page.locator('[id="crucible.run-files"]');
    await expect(list.getByRole('button', { name: 'summarize[flashinfer]/SUMMARY.md' })).toBeVisible();
    await expect(list.getByTestId('failure-evidence')).toHaveCount(1);
    await expect(
      list.getByRole('button', { name: 'summarize[flashinfer]/SUMMARY.md' }).getByTestId('failure-evidence'),
    ).toBeVisible();
    await expect(
      list.getByRole('button', { name: 'summarize[paged-attention]/SUMMARY.md' }).getByTestId('failure-evidence'),
    ).toHaveCount(0);

    await list.getByRole('button', { name: 'summarize[flashinfer]/SUMMARY.md' }).click();
    await expect(page.getByText('captured from a task that failed')).toBeVisible();
  });

  test('reads a captured file out of a finished run', async ({ page }) => {
    await stubApi(page);
    await ready(page, `/runs/${encodeURIComponent(TRIAGE_RUN)}`);

    const graph = page.getByTestId('workflow-graph');
    await graph.locator('[data-task="triage[1027]"]').click();

    const panel = page.getByRole('complementary', { name: 'Task triage[1027]' });
    const content = panel.getByTestId('task-evidence-content');
    await expect(panel.getByTestId('task-payload')).toContainText('"classification": "feature"');

    const file = panel.getByTestId('task-file');
    await expect(file).toContainText('TRIAGE.md');
    await expect(file).toContainText('1.3 KB');
    await file.click();
    await expect(content).toContainText('feature, severity low, confidence high');

    await panel.getByRole('button', { name: 'run log' }).click();
    await expect(panel.getByTestId('run-log')).toContainText('admitted 3 tasks');
  });

  /// A task the run has not finished says so, instead of reading as a finished one with nothing to
  /// show.
  test('says a task is still running rather than showing empty evidence', async ({ page }) => {
    await stubApi(page);
    await ready(page, `/runs/${encodeURIComponent(TRIAGE_RUN)}`);

    await page.getByTestId('workflow-graph').locator('[data-task="scan"]').click();
    const panel = page.getByRole('complementary', { name: 'Task scan' });
    await expect(panel.getByTestId('task-pending')).toHaveText('running — no result yet');
    await expect(panel.getByTestId('task-file')).toHaveCount(0);
  });
});

test.describe('the run task grid', () => {
  /// The grid holds what the graph drops: every attempt, in the column of the iteration it came
  /// from. `read` failed on iter 0 and passed on iter 1, and both squares are there to see.
  test('draws every attempt as tasks down and iterations across', async ({ page }) => {
    await stubApi(page);
    await ready(page, RUN);

    const grid = page.getByTestId('run-grid');
    await expect(grid).toBeVisible();

    await expect(grid.locator('[data-task="read"]')).toHaveCount(2);
    await expect(grid.locator('[data-task="read"][data-iter="0"]')).toHaveAttribute(
      'data-status',
      'fail',
    );
    await expect(grid.locator('[data-task="read"][data-iter="1"]')).toHaveAttribute(
      'data-status',
      'pass',
    );

    // Dependencies come before the tasks that wait on them, and an instance hangs off its task.
    const names = await grid.locator('[data-task-row]').evaluateAll((rows) =>
      rows.map((row) => row.getAttribute('data-task-row')),
    );
    expect(names).toEqual([
      'read',
      'summarize',
      'summarize[paged-attention]',
      'summarize[flashinfer]',
      'rank',
      'file',
    ]);

    // A task that never reported keeps its row, with no cell in either column.
    await expect(grid.locator('[data-task="file"]')).toHaveCount(0);
  });

  /// The row totals sum across attempts, so the task a run spent its time in is the widest bar.
  test('totals duration and cost across a retried task', async ({ page }) => {
    await stubApi(page);
    await ready(page, RUN);

    const grid = page.getByTestId('run-grid');
    const row = grid.locator('[data-task-row="read"]');
    // 31s on iter 0 plus 240s on iter 1.
    await expect(row.locator('xpath=following-sibling::*[1]')).toHaveAttribute('title', '4m 31s');

    await expect(page.getByText('pass 3', { exact: true })).toBeVisible();
    await expect(page.getByText('fail 2', { exact: true })).toBeVisible();
  });

  /// Picking a cell reads that task's evidence without leaving the grid.
  test('opens a picked attempt and its evidence', async ({ page }) => {
    await stubApi(page);
    await ready(page, RUN);

    await page
      .getByTestId('run-grid')
      .locator('[data-task="summarize[flashinfer]"][data-iter="1"]')
      .click();

    // The picked cell says what the attempt reported, and its evidence opens beneath.
    await expect(page.getByRole('paragraph').filter({ hasText: 'no citations parsed' })).toBeVisible();
    await expect(page.getByTestId('task-result')).toContainText('at summarize.sh:14');
  });
});
