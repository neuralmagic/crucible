import { expect, test, type Page, type Route } from '@playwright/test';
import { RUN_FILE_CSP } from '../src/pages/runFileView';
import {
  COST_DRIFT_RUN,
  PLANLESS_RUN,
  PLAYBOOK_RUN,
  RETRY_RUN,
  ROUTE_RUN,
  RUNS,
  stubApi,
  TRIAGE_RUN,
} from './api';

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
    // Instances are rows under their mapped task, which places them with no edge of their own, and
    // become the producers for its downstream edge; the fifth edge reaches the marker for a
    // revision that stored no exposure.
    await expect(graph.locator('.react-flow__edges path.react-flow__edge-path')).toHaveCount(5);

    const read = graph.locator('[data-task="read"]');
    await expect(read).toContainText('pass');
    await expect(read).toContainText('⟳ survey');
    // Two attempts, summed cost and duration.
    await expect(read).toContainText('iter 1 · 2 attempts · $1.50 · 4m 31s');
    await expect(graph.locator('[data-task="file"]')).not.toContainText('iter');

    const failed = graph.locator('[data-task="summarize[flashinfer]"]');
    await expect(failed).toContainText('fail');
    await expect(failed).toHaveAttribute('data-status', 'fail');
    await expect(failed.locator('[data-mark="fail"]')).toBeVisible();
    await expect(graph.locator('[data-task="summarize[paged-attention]"]')).toContainText('pass');
    await expect(read.locator('[data-mark="pass"]')).toBeVisible();
    // The status is the colour and the dot, not a word; the glyph says what runs the task.
    await expect(read.getByText('pass', { exact: true })).toBeHidden();
    await expect(page.getByText('red = failed')).toHaveCount(0);
  });

  /// A mapped route reads as a decision, tallies what its instances answered, and the fix aligned
  /// with it counts the elements its when left out apart from the one that failed.
  test('draws a mapped route, its answers, and the per-element edges it feeds', async ({ page }) => {
    await stubApi(page);
    await ready(page, `/runs/${ROUTE_RUN}`);

    const graph = page.getByTestId('workflow-graph');
    const route = graph.locator('[data-task="triage"]');
    await expect(route).toContainText('DECIDE');
    await expect(route).toContainText('over scan.items ≤120');
    await expect(route).toContainText('tier: skip 2 · high 1 · low 1');
    await expect(graph.locator('[data-task="triage[a]"]')).toContainText('high');
    await expect(graph.locator('[data-task="fix"]')).toContainText(
      '4 · 1 passed · 2 not taken · 1 failed'
    );
    // Each paired edge carries the answer that let its instance through; the deck-to-deck edge,
    // which would say it again, is not drawn.
    await expect(graph.locator('[data-edge-label]').filter({ hasText: 'tier: high' }).first()).toBeVisible();
    await expect(graph.locator('[data-edge-label]').filter({ hasText: 'tier: high|low' })).toHaveCount(0);
    // What an edge carries shows only while it is traced.
    await expect(graph.locator('[data-plumbing]')).toHaveCount(0);
    await graph.locator('[data-task="fix"]').hover();
    await expect(graph.locator('[data-plumbing]').filter({ hasText: 'notes[item]' }).first()).toBeVisible();
    await page.mouse.move(0, 0);
    await expect(graph.locator('[data-plumbing]')).toHaveCount(0);
    await expect(page.getByText('double line = per element')).toHaveCount(0);

    await route.click();
    const panel = page.getByRole('complementary', { name: 'Task triage' });
    await expect(panel).toContainText('narrows');
    await expect(panel).toContainText('scan.notes');
  });

  /// A mapped task is the fan-out, not the work: it says MAP, and the instances under it say what
  /// they actually run.
  test('badges a mapped task as a map and its instances as what they run', async ({ page }) => {
    await stubApi(page);
    await ready(page, RUN);

    const graph = page.getByTestId('workflow-graph');
    const deck = graph.locator('[data-task="summarize"]');
    await expect(deck).toContainText('MAP');
    await expect(deck).toContainText('over read.papers ≤8');
    await expect(graph.locator('[data-task="summarize[flashinfer]"]')).toContainText('CMD');
    await expect(graph.locator('[data-task="read"]')).toContainText('AGENT');
    await expect(page.getByText('MAP = mapped over a producer field')).toHaveCount(0);
  });

  /// The case the badge exists for: a mapped agent task whose deck runs nothing itself.
  test('badges a mapped agent task as a map, not as an agent', async ({ page }) => {
    await stubApi(page);
    await ready(page, `/runs/${encodeURIComponent(TRIAGE_RUN)}`);

    const graph = page.getByTestId('workflow-graph');
    await expect(graph.locator('[data-task="triage"]')).toContainText('MAP');
    await expect(graph.locator('[data-task="triage[1027]"]')).toContainText('AGENT');
    await expect(graph.locator('[data-task="scan"]')).toContainText('AGENT');
  });

  /// A mapped task says what the run made of it: three papers went in, two instances started, one
  /// of those failed. The run is still going, so what it has not reached is pending, not missed.
  test('counts a mapped task against the items it was spread over', async ({ page }) => {
    await stubApi(page);
    await ready(page, RUN);

    const graph = page.getByTestId('workflow-graph');
    await expect(graph.locator('[data-fanout="summarize"]')).toHaveText(
      '3 · 1 passed · 1 failed · 1 pending',
    );
    await expect(graph.locator('[data-task="file"]')).toContainText('pending');

    await graph.locator('[data-task="summarize"]').click();
    const panel = page.getByRole('complementary', { name: 'Task summarize' });
    const value = (label: string) =>
      panel.locator('dt', { hasText: new RegExp(`^${label}$`) }).locator('+ dd');
    await expect(value('items')).toHaveText('3');
    await expect(value('started')).toHaveText('2');
    await expect(value('passed')).toHaveText('1');
    await expect(value('fail')).toHaveText('1');
  });

  /// The case the panel exists for: twenty tickets were handed to `triage` and only two needed an
  /// agent, which the result rows alone cannot show.
  test('shows a fan-out most of whose items never started', async ({ page }) => {
    await stubApi(page);
    await ready(page, `/runs/${encodeURIComponent(TRIAGE_RUN)}`);

    const graph = page.getByTestId('workflow-graph');
    await expect(graph.locator('[data-fanout="triage"]')).toHaveText(
      '20 · 2 passed · 18 never started',
    );
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

  /// A captured html page, such as a build trace, renders in place the way flow.html does: in an
  /// iframe sandboxed to scripts alone, so the page runs but cannot reach the controller.
  test('renders a captured html page in a script-only sandbox', async ({ page }) => {
    await stubApi(page);
    const html =
      '<!doctype html><html><head><title>trace</title></head><body><p id="t">static</p>' +
      "<script>document.getElementById('t').textContent = 'scripted trace';</script></body></html>";
    await page.route('**/api/runs/RUN-0412/files', (route) =>
      route.fulfill({
        json: {
          run_id: 'RUN-0412',
          files: [
            { task: 'rank', instance: null, path: 'RANKING.md', size_bytes: 210, key: 'rank/RANKING.md' },
            { task: 'trace', instance: null, path: 'build-trace.html', size_bytes: html.length, key: 'trace/build-trace.html' },
          ],
        },
      }),
    );
    await page.route('**/api/runs/RUN-0412/files/trace/build-trace.html', (route) =>
      route.fulfill({ contentType: 'application/octet-stream', body: html }),
    );
    await ready(page, '/runs/RUN-0412');

    const list = page.locator('[id="crucible.run-files"]');
    await list.getByRole('button', { name: 'trace/build-trace.html' }).click();
    const frame = page.locator('iframe[title="trace/build-trace.html"]');
    await expect(frame).toHaveAttribute('sandbox', 'allow-scripts');
    await expect(frame.contentFrame().locator('#t')).toHaveText('scripted trace');
    await expect(frame.contentFrame().locator('meta[http-equiv="Content-Security-Policy"]')).toHaveCount(1);
  });

  test('reads a captured file out of a finished run', async ({ page }) => {
    await stubApi(page);
    await ready(page, `/runs/${encodeURIComponent(TRIAGE_RUN)}`);

    const graph = page.getByTestId('workflow-graph');
    await graph.locator('[data-task="triage[1027]"]').click();

    const panel = page.getByRole('complementary', { name: 'Task triage[1027]' });
    const content = panel.getByTestId('task-evidence-content');
    await expect(panel.getByTestId('task-payload')).toContainText('"classification": "feature"');

    const file = panel.getByTestId('task-file').filter({ hasText: 'TRIAGE.md' });
    await expect(file).toContainText('1.3 KB');
    await file.click();
    await expect(content).toContainText('feature, severity low, confidence high');

    await panel.getByRole('button', { name: 'run log' }).click();
    await expect(panel.getByTestId('run-log')).toContainText('admitted 3 tasks');
  });

  /// A task's html capture opens from the panel as the document it is — fetched from the file store
  /// when the payload did not carry it, sandboxed the way the Files tab sandboxes one.
  test('opens a captured html report from the task panel', async ({ page }) => {
    await stubApi(page);
    const html =
      '<!doctype html><html><head><title>report</title></head><body><p id="t">static</p>' +
      "<script>document.getElementById('t').textContent = 'scripted report';</script></body></html>";
    await page.route(/campaign-report\.html$/, (route) =>
      route.fulfill({ contentType: 'application/octet-stream', body: html }),
    );
    await ready(page, `/runs/${encodeURIComponent(TRIAGE_RUN)}`);

    await page.getByTestId('workflow-graph').locator('[data-task="triage[1027]"]').click();
    const panel = page.getByRole('complementary', { name: 'Task triage[1027]' });
    const report = panel.getByTestId('task-file').filter({ hasText: 'campaign-report.html' });
    await expect(report).toContainText('1.4 MB');
    await expect(report).not.toContainText('not shown inline');
    await report.click();

    const frame = panel.locator('iframe[title="evidence/campaign-report.html"]');
    await expect(frame).toHaveAttribute('sandbox', 'allow-scripts');
    await expect(frame.contentFrame().locator('#t')).toHaveText('scripted report');
  });

  /// Program output keeps its colours, in the theme's palette, and none of its escape codes.
  test('reads the run log in the log viewer with its ANSI colours in the theme palette', async ({ page }) => {
    await stubApi(page);
    const text =
      '\x1b[2m2026-11-04T20:04:14Z\x1b[0m \x1b[32m INFO\x1b[0m \x1b[1mserve_inner\x1b[0m\x1b[2m:\x1b[0m task cancelled\n' +
      'plan run: admitted 3 tasks\n';
    await page.route(`**/api/runs/${TRIAGE_RUN}/log`, (route) =>
      route.fulfill({ json: { run_id: TRIAGE_RUN, dispatch: 'local', text, truncated: false, location: null } }),
    );
    await ready(page, `/runs/${encodeURIComponent(TRIAGE_RUN)}`);
    await page.getByTestId('workflow-graph').locator('[data-task="triage[1027]"]').click();
    const panel = page.getByRole('complementary', { name: 'Task triage[1027]' });
    await panel.getByRole('button', { name: 'run log' }).click();

    const log = panel.getByTestId('run-log');
    await expect(log).toContainText('serve_inner: task cancelled');
    await expect(log).toContainText('admitted 3 tasks');
    await expect(log).not.toContainText('[2m');
    const info = log.locator('[class*="index-module_green__"]');
    const body = log.locator('[class*="index-module_lazyLog__"]');
    await expect(info).toHaveText(' INFO');
    await expect(info).toHaveCSS('color', 'rgb(90, 203, 134)');
    await expect(body).toHaveCSS('background-color', 'rgb(10, 10, 9)');

    await log.getByRole('textbox').fill('cancelled');
    await expect(log.locator('[class*="index-module_searchMatch"]').first()).toHaveText('cancelled');

    await page.evaluate(() => {
      document.documentElement.dataset.theme = 'light';
    });
    await expect(info).toHaveCSS('color', 'rgb(0, 102, 58)');
    await expect(body).toHaveCSS('background-color', 'rgb(237, 235, 226)');
  });

  test('reads a captured log file in the log viewer', async ({ page }) => {
    await stubApi(page);
    const body = '\x1b[31merror\x1b[0m: kernel_selector.h: No such file or directory\n[127/130] Building CUDA object\n';
    await page.route('**/api/runs/RUN-0412/files', (route) =>
      route.fulfill({
        json: {
          run_id: 'RUN-0412',
          files: [{ task: 'build', instance: null, path: 'edit1.log', size_bytes: body.length, key: 'build/edit1.log' }],
        },
      }),
    );
    await page.route('**/api/runs/RUN-0412/files/build/edit1.log', (route) =>
      route.fulfill({ contentType: 'application/octet-stream', body }),
    );
    await ready(page, '/runs/RUN-0412');

    await page.locator('[id="crucible.run-files"]').getByRole('button', { name: 'build/edit1.log' }).click();
    const view = page.getByTestId('run-file-log');
    await expect(view).toContainText('error: kernel_selector.h: No such file or directory');
    await expect(view).toContainText('[127/130] Building CUDA object');
    await expect(view.locator('[class*="index-module_red__"]')).toHaveCSS('color', 'rgb(229, 107, 105)');
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

const openGrid = (page: Page) => page.locator('summary', { hasText: 'Grid' }).click();

test.describe('the run task grid', () => {
  /// The grid holds what the graph drops: every attempt, in the column of the iteration it came
  /// from. `read` failed on iter 0 and passed on iter 1, and both squares are there to see.
  test('draws every attempt as tasks down and iterations across', async ({ page }) => {
    await stubApi(page);
    await ready(page, RUN);
    await openGrid(page);

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
    await openGrid(page);

    const grid = page.getByTestId('run-grid');
    const row = grid.locator('[data-task-row="read"]');
    // 31s on iter 0 plus 240s on iter 1.
    await expect(row.locator('xpath=following-sibling::*[1]')).toHaveAttribute('title', '4m 31s');

    const summary = page.locator('summary', { hasText: 'Grid' });
    await expect(summary).toContainText('5 attempts · 2 failed · 3 passed');
    await expect(summary.getByText('· 2 failed')).toHaveClass(/text-red/);
  });

  /// What an agent instance actually ran with, beside the attempt it ran. The mapped parent runs
  /// nothing itself, so it reports a value only where its instances agreed on one, and a command
  /// task reports none at all.
  test('names the provider, model and effort each agent task ran with', async ({ page }) => {
    await stubApi(page);
    await ready(page, `/runs/${encodeURIComponent(TRIAGE_RUN)}`);
    await openGrid(page);

    const grid = page.getByTestId('run-grid');
    const cell = (task: string, col: string) =>
      grid.locator(`[data-task-agent="${task}"][data-col="${col}"]`);

    await expect(cell('scan', 'provider')).toHaveText('pricetag-glm');
    await expect(cell('scan', 'model')).toHaveText('glm-5.3');
    await expect(cell('scan', 'effort')).toHaveText('low');

    await expect(cell('triage[952]', 'model')).toHaveText('glm-5.3');
    await expect(cell('triage[952]', 'effort')).toHaveText('high');

    await expect(cell('triage', 'model')).toHaveText('glm-5.3');
    await expect(cell('triage', 'effort')).toHaveText('\u2014');

    for (const col of ['provider', 'model', 'effort']) {
      await expect(cell('roundup', col)).toHaveText('');
    }
  });

  /// A task that reported the same status nine iterations running is drawn as three blocks, not
  /// nine squares: the stretches carry their length and the failure keeps its own column.
  test('melds a run of attempts that ended the same way', async ({ page }) => {
    await stubApi(page);
    await ready(page, `/runs/${RETRY_RUN}`);
    await openGrid(page);

    const row = page.getByTestId('run-grid').locator('[data-task="probe"]');
    await expect(row).toHaveCount(3);
    await expect(row).toHaveText(['5', '', '3']);
    await expect(row.nth(0)).toHaveAttribute('data-status', 'pass');
    await expect(row.nth(0)).toHaveAttribute('data-iter', '0');
    await expect(row.nth(1)).toHaveAttribute('data-status', 'fail');
    await expect(row.nth(1)).toHaveAttribute('data-span', '1');
    await expect(row.nth(2)).toHaveAttribute('data-iter', '6');
    await expect(row.nth(2)).toHaveAttribute('data-span', '3');
  });

  /// Picking a melded block opens the first attempt under it, not the last.
  test('opens the first attempt of a melded block', async ({ page }) => {
    await stubApi(page);
    await ready(page, `/runs/${RETRY_RUN}`);
    await openGrid(page);

    await page.getByTestId('run-grid').locator('[data-task="probe"]').nth(2).click();
    await expect(page.getByText('iter 6', { exact: true })).toBeVisible();
  });

  /// Picking a cell reads that task's evidence without leaving the grid.
  test('opens a picked attempt and its evidence', async ({ page }) => {
    await stubApi(page);
    await ready(page, RUN);
    await openGrid(page);

    await page
      .getByTestId('run-grid')
      .locator('[data-task="summarize[flashinfer]"][data-iter="1"]')
      .click();

    // The picked cell says what the attempt reported, and its evidence opens beneath.
    await expect(page.getByRole('paragraph').filter({ hasText: 'no citations parsed' })).toBeVisible();
    await expect(page.getByTestId('task-result')).toContainText('at summarize.sh:14');
  });
});

test.describe('the run header', () => {
  /// The run id is cut to fit the header line and copies whole.
  test('copies the whole run id from its cut chip', async ({ page, context }) => {
    await context.grantPermissions(['clipboard-read', 'clipboard-write']);
    await stubApi(page);
    await ready(page, RUN);

    const chip = page.getByRole('button', { name: 'Copy RUN-0412' });
    await expect(chip).toHaveAttribute('title', 'RUN-0412');
    await chip.click();
    await expect(chip).toHaveText(/copied/);
    expect(await page.evaluate(() => navigator.clipboard.readText())).toBe('RUN-0412');
  });

  /// A running playbook run's header carries its launch's stop: the cancel is armed in two steps
  /// and posts the launch's key, so a run watched from its own page can be ended there.
  test('offers the launch cancel from a running run header', async ({ page }) => {
    await stubApi(page);
    await page.route(`**/api/runs/${TRIAGE_RUN}`, (route) =>
      route.fulfill({
        status: 200,
        json: {
          run: {
            ...RUNS[1],
            run_id: TRIAGE_RUN,
            issue_key: 'playbook:triage-local:01a030fe',
            status: 'running',
            cost_usd: null,
            pod: null,
          },
          candidates: [],
          has_plan: true,
        },
      }),
    );
    await page.route('**/api/playbook-runs/playbook%3Atriage-local%3A01a030fe', (route) =>
      route.fulfill({
        status: 200,
        json: {
          launch: {
            ...PLAYBOOK_RUN,
            key: 'playbook:triage-local:01a030fe',
            playbook: 'triage-local',
            status: 'running',
            cost_usd: null,
          },
          source_exists: true,
          dispatch: { state: 'dispatched', failure: null, failed_at: null, failures: 0 },
          runs: [],
        },
      }),
    );
    let cancelledUrl = '';
    await page.route(
      '**/api/playbook-runs/playbook%3Atriage-local%3A01a030fe/cancel',
      (route) => {
        cancelledUrl = route.request().url();
        return route.fulfill({
          status: 202,
          json: {
            key: 'playbook:triage-local:01a030fe',
            actor: 'kylesayrs',
            reason: 'cancelled by kylesayrs',
          },
        });
      },
    );
    await ready(page, `/runs/${encodeURIComponent(TRIAGE_RUN)}`);

    const cancel = page.getByRole('button', { name: 'Cancel', exact: true });
    await expect(cancel).toBeVisible();
    await cancel.click();
    await page.getByRole('button', { name: 'Confirm cancel', exact: true }).click();
    await expect(page.getByText('Cancelling')).toBeVisible();
    expect(cancelledUrl).toContain('playbook%3Atriage-local%3A01a030fe/cancel');
  });
});

test.describe('external results', () => {
  const sectionOf = (page: Page, header: string) =>
    page
      .locator('[data-ui="section"]')
      .filter({ has: page.locator('[data-ui="section-header"]', { hasText: header }) });

  /// What the run produced outside itself reaches the page as a link per result, each under the
  /// mark of the service it points at.
  test('lists every link the run reported, with its provider mark', async ({ page }) => {
    await stubApi(page);
    await ready(page, RUN);

    const section = page.getByTestId('run-links');
    await expect(section.getByRole('link')).toHaveCount(6);

    const pr = section.getByRole('link', { name: /#412/ });
    await expect(pr).toHaveAttribute('href', 'https://github.com/neuralmagic/crucible/pull/412');
    await expect(pr).toHaveAttribute('target', '_blank');
    await expect(pr).toHaveAttribute('rel', 'noopener noreferrer');
    await expect(pr.locator('svg[aria-label="GitHub"]')).toBeVisible();
    await expect(
      section.getByRole('link', { name: /INFERENG-77/ }).locator('svg[aria-label="Jira"]'),
    ).toBeVisible();
  });

  /// The task that reported them says so too: picking its cell lists that attempt's links.
  test('shows a picked attempt its own links', async ({ page }) => {
    await stubApi(page);
    await ready(page, RUN);
    await openGrid(page);

    const attempts = sectionOf(page, 'Grid');
    await page.getByTestId('run-grid').locator('[data-task="rank"][data-iter="1"]').click();
    await expect(attempts.getByRole('link', { name: /survey-412/ })).toHaveCount(1);

    await page.getByTestId('run-grid').locator('[data-task="summarize[flashinfer]"][data-iter="1"]').click();
    await expect(attempts.getByRole('link', { name: /survey-412/ })).toHaveCount(0);
  });

  /// A task's node carries what it opened: the mark of the service and the label, linking out
  /// from the graph itself.
  test('draws a task its own links on its node', async ({ page }) => {
    await stubApi(page);
    await ready(page, RUN);

    const node = page.getByTestId('workflow-graph').locator('[data-node-links="rank"]');
    const pr = node.getByRole('link', { name: /#412/ });
    await expect(pr).toHaveAttribute('href', 'https://github.com/neuralmagic/crucible/pull/412');
    await expect(pr).toHaveAttribute('target', '_blank');
    await expect(pr).toHaveAttribute('rel', 'noopener noreferrer');
    await expect(pr.locator('svg[aria-label="GitHub"]')).toBeVisible();
    await expect(node.getByRole('link')).toHaveCount(2);
    await expect(node).toContainText('+1');
  });

  /// A mapped task has as many links as the run spread it over, so its deck carries the tally and
  /// the instances carry the links.
  test('tallies a mapped task and leaves its instances their own links', async ({ page }) => {
    await stubApi(page);
    await ready(page, RUN);

    const graph = page.getByTestId('workflow-graph');
    const deck = graph.locator('[data-node-links="summarize"]');
    await expect(deck.locator('span')).toHaveText(['GitHub2', 'GitLab1']);
    await expect(deck.locator('svg[aria-label="GitHub"]')).toBeVisible();
    await expect(deck.locator('svg[aria-label="GitLab"]')).toBeVisible();
    await expect(deck.getByRole('link')).toHaveCount(0);

    const instance = graph.locator('[data-node-links="summarize[paged-attention]"]');
    await expect(instance.getByRole('link', { name: /#418/ })).toHaveAttribute(
      'href',
      'https://github.com/neuralmagic/crucible/pull/418',
    );
    await expect(
      graph.locator('[data-node-links="summarize[flashinfer]"]').getByRole('link', { name: /!9/ }),
    ).toHaveAttribute('href', 'https://gitlab.com/vllm/kernels/-/merge_requests/9');
  });

  /// A task that reported nothing outside the run gets no row at all.
  test('draws no marks on a task that reported no links', async ({ page }) => {
    await stubApi(page);
    await ready(page, RUN);

    const graph = page.getByTestId('workflow-graph');
    await expect(graph.locator('[data-node-links="read"]')).toHaveCount(0);
    await expect(graph.locator('[data-node-links="file"]')).toHaveCount(0);
  });
});

test.describe('a plan-less run', () => {
  /// Every run from before the work-graph executor: the detail says it never admitted a plan and
  /// the artifacts listing carries no flow.html. The page answers both from what it already
  /// fetched — the graph and flow endpoints' 404s are the same answers, but console-noisy ones,
  /// so neither may be requested at all.
  test('draws its score surface without probing the graph or flow endpoints', async ({ page }) => {
    await stubApi(page);
    // Registered after the stub, so these win: the real controller's answers, planted to make a
    // probe loud in the console rather than silently absorbed.
    for (const suffix of ['graph', 'artifacts/flow.html']) {
      await page.route(`**/api/runs/${PLANLESS_RUN}/${suffix}`, (route) =>
        route.fulfill({ status: 404, body: 'not there' }),
      );
    }
    const consoleErrors: string[] = [];
    page.on('console', (message) => {
      if (message.type() === 'error') consoleErrors.push(message.text());
    });
    const probes: string[] = [];
    page.on('request', (request) => {
      const url = request.url();
      if (url.endsWith(`/api/runs/${PLANLESS_RUN}/graph`) || url.endsWith(`/${PLANLESS_RUN}/artifacts/flow.html`)) {
        probes.push(url);
      }
    });

    await ready(page, `/runs/${PLANLESS_RUN}`);

    await expect(page.getByTestId('workflow-graph')).toHaveCount(0);
    await expect(page.getByText('No visual report for this run.')).toBeVisible();
    await expect(page.getByText('No measured iterations to chart.')).toBeVisible();
    expect(probes, 'the page asked an endpoint it could have answered locally').toEqual([]);
    expect(consoleErrors, 'console errors on a plan-less run').toEqual([]);
  });
});

test.describe('a run whose iteration costs drift under rounding', () => {
  /// Each of the three iteration costs rounds up on its own, so the raw cells miss the row total
  /// by a cent ($0.13 + $0.21 + $0.31 against $0.64). The table prints line items that reach
  /// the total beside them, and the run total under them.
  test('reconciles cost line items with the total they report', async ({ page }) => {
    await stubApi(page);
    await ready(page, `/runs/${encodeURIComponent(COST_DRIFT_RUN)}`);

    const section = page.locator('section[data-ui="section"]', {
      has: page.locator('[data-ui="section-header"]', { hasText: 'Cost' }),
    });
    const row = section.getByRole('row', { name: /propose/ });
    await expect(row).toBeVisible();
    await expect(row.locator('td')).toHaveText(['propose', '$0.12', '$0.21', '$0.31', '$0.64', '']);
    await expect(section.getByText('run total $0.64')).toBeVisible();
  });
});

test.describe('the cost table beside the artifacts table', () => {
  /// The section's border-r divides the cost table from the artifacts table beside it; the
  /// table's own right edge would paint a second line against it. Below the wide breakpoint the
  /// panels stack, the section drops its border, and the table's edge returns.
  test('draws one rule between the cost and artifacts tables', async ({ page }) => {
    await stubApi(page);
    await ready(page, `/runs/${encodeURIComponent(COST_DRIFT_RUN)}`);

    const section = page.locator('section[data-ui="section"]', {
      has: page.locator('[data-ui="section-header"]', { hasText: 'Cost' }),
    });
    const shareHeader = section.getByRole('columnheader', { name: 'Share' });
    await expect(shareHeader).toHaveCSS('border-right-width', '0px');
    await expect(section.getByRole('row', { name: /propose/ }).locator('td').last()).toHaveCSS(
      'border-right-width',
      '0px',
    );
    await expect(section).toHaveCSS('border-right-width', '1px');

    await page.setViewportSize({ width: 900, height: 1000 });
    await expect(shareHeader).toHaveCSS('border-right-width', '1px');
    await expect(section).toHaveCSS('border-right-width', '0px');
  });
});

const LOCAL_RUN = 'playbook_bench_01a1-1791474738';
const REPORT_KEY = 'report/index.html';

/** A report whose own script draws it, then tries the page around it and the network. */
const REPORT_HTML = `<!DOCTYPE html><html><head><title>report</title></head><body>
<h1 id="drawn">not drawn</h1><p id="reach">reached the page</p><p id="net">no request</p>
<script>
document.getElementById('drawn').textContent = 'drawn by its own script';
try { void parent.document.title; } catch { document.getElementById('reach').textContent = 'isolated'; }
document.addEventListener('securitypolicyviolation', (e) => {
  document.getElementById('net').textContent = 'blocked by ' + e.effectiveDirective;
});
fetch('/api/whoami').catch(() => {});
</script></body></html>`;

const planTask = (name: string, dependsOn: string[]) => ({
  name,
  kind: 'command',
  depends_on: dependsOn,
  session: '',
  needs: 'any',
  required: true,
  over: '',
  max_fanout: 0,
  when: '',
  keyed: [],
});

const passed = (task: string) => ({
  iter: 0,
  task,
  status: 'pass',
  note: '',
  cost_usd: null,
  secs: 0,
  links: [],
  repairs: [],
  agent: null,
});

const captured = (task: string, path: string, sizeBytes: number) => ({
  task,
  instance: null,
  path,
  size_bytes: sizeBytes,
  key: `${task}/${path}`,
});

/** What the run has done when the next request lands. */
interface LocalRun {
  built: boolean;
  finished: boolean;
  landed: boolean;
}

async function stubLocalRun(page: Page, run: LocalRun): Promise<void> {
  await page.route(
    (url: URL) => url.pathname.startsWith(`/api/runs/${LOCAL_RUN}`),
    async (route: Route) => {
      const path = decodeURIComponent(new URL(route.request().url()).pathname);
      const json = (body: unknown) =>
        route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(body) });
      switch (path.slice(`/api/runs/${LOCAL_RUN}`.length)) {
        case '': {
          if (run.finished && !run.landed) {
            await new Promise((resolve) => setTimeout(resolve, 300));
            run.landed = true;
          }
          return json({
            run: {
              run_id: LOCAL_RUN,
              scope: 4,
              cluster: 'hub',
              namespace: 'autoresearch',
              issue_key: 'playbook:bench:01a1',
              repo: null,
              status: run.finished ? 'succeeded' : 'running',
              dispatch: 'local',
              best_score: null,
              cost_usd: null,
              score_series: [],
              identity_digest: null,
              pod: null,
              created: '2026-11-04T18:00:00Z',
            },
            candidates: [],
          });
        }
        case '/iterations':
          return json([]);
        case '/graph':
          return json({
            plan_version: 1,
            tasks: [planTask('build', []), planTask('report', ['build'])],
            fanout: [],
            decisions: [],
            results: [
              ...(run.built ? [passed('build')] : []),
              ...(run.landed ? [passed('report')] : []),
            ],
          });
        case '/files':
          return json({
            run_id: LOCAL_RUN,
            files: [
              ...(run.built ? [captured('build', 'BUILD.md', 24)] : []),
              ...(run.landed ? [captured('report', 'index.html', REPORT_HTML.length)] : []),
            ],
          });
        case '/files/build/BUILD.md':
          return route.fulfill({ status: 200, contentType: 'application/octet-stream', body: '# build\n\nwheel built' });
        case `/files/${REPORT_KEY}`:
          return route.fulfill({ status: 200, contentType: 'application/octet-stream', body: REPORT_HTML });
        default:
          return route.fallback();
      }
    },
  );
}

test.describe('a playbook run on this machine', () => {
  /// A captured HTML report runs its own script, but cannot reach the page or the network.
  test('renders a captured HTML report in an isolated frame', async ({ page }) => {
    await stubApi(page);
    await stubLocalRun(page, { built: true, finished: true, landed: true });
    await ready(page, `/runs/${LOCAL_RUN}/files/${REPORT_KEY}`);

    const frame = page.locator(`iframe[title="${REPORT_KEY}"]`);
    await expect(frame).toHaveAttribute('sandbox', 'allow-scripts');
    await expect(frame).toHaveAttribute('referrerpolicy', 'no-referrer');
    const srcdoc = (await frame.getAttribute('srcdoc')) ?? '';
    expect(srcdoc.slice(0, srcdoc.indexOf('>') + 1)).toBe(
      `<meta http-equiv="Content-Security-Policy" content="${RUN_FILE_CSP}">`,
    );
    expect(RUN_FILE_CSP).toContain("default-src 'none'");
    const report = frame.contentFrame();
    await expect(report.locator('#drawn')).toHaveText('drawn by its own script');
    await expect(report.locator('#reach')).toHaveText('isolated');
    await expect(report.locator('#net')).toHaveText('blocked by connect-src');
  });

  /// A task that finishes and the file it captured show up without a reload.
  test('shows results and captures that land while it runs', async ({ page }) => {
    const run = { built: false, finished: false, landed: false };
    await page.clock.install();
    await stubApi(page);
    await stubLocalRun(page, run);
    await ready(page, `/runs/${LOCAL_RUN}`);

    const build = page.getByTestId('workflow-graph').locator('[data-task="build"]');
    await expect(build).toHaveAttribute('data-status', 'pending');
    await expect(page.getByText('No files captured for this run.')).toBeVisible();

    run.built = true;
    await page.clock.fastForward(5_000);
    await expect(build).toHaveAttribute('data-status', 'pass');
    await expect(
      page.locator('[id="crucible.run-files"]').getByRole('button', { name: 'build/BUILD.md' }),
    ).toBeVisible();
  });

  /// A finished run shows its report and does not say a task that ran never did.
  test('fetches its last captures and results once it stops', async ({ page }) => {
    const run = { built: true, finished: false, landed: false };
    await page.clock.install();
    await stubApi(page);
    await stubLocalRun(page, run);
    await ready(page, `/runs/${LOCAL_RUN}`);

    const report = page.getByTestId('workflow-graph').locator('[data-task="report"]');
    await expect(report).toHaveAttribute('data-status', 'pending');

    run.finished = true;
    await page.clock.fastForward(5_000);
    await expect(report).toHaveAttribute('data-status', 'pass');
    await expect(
      page.locator('[id="crucible.run-files"]').getByRole('button', { name: REPORT_KEY }),
    ).toBeVisible();
  });
});

test.describe('a run file deep link', () => {
  /// The files panel scrolls itself into view, and stops once the reader scrolls.
  test('leaves the scroll to the reader once they scroll', async ({ page }) => {
    await page.addInitScript(() => {
      document.addEventListener('DOMContentLoaded', () => {
        const style = document.createElement('style');
        style.textContent = '[data-ui="main"] div:has([id="crucible.run-files"]) ~ * { display: none !important; }';
        document.head.append(style);
      });
    });
    await stubApi(page);
    await page.goto(`${RUN}/files/rank/RANKING.md`);

    const scrollTop = () => page.locator('main').evaluate((main) => main.scrollTop);
    await expect.poll(scrollTop).toBeGreaterThan(0);

    const box = await page.locator('main').boundingBox();
    if (box === null) throw new Error('main has no box');
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await page.mouse.wheel(0, -10_000);
    await expect.poll(scrollTop).toBe(0);
    await page.waitForTimeout(500);
    expect(await scrollTop()).toBe(0);
  });
});
