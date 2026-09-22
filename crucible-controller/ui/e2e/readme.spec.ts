import { expect, test, type Page } from '@playwright/test';
import { ROUTES, stubApi } from './api';

// README screenshots. Not part of the suite: set README_SHOTS=1 to run it, and it writes the
// pages under docs/img/ rather than comparing against a baseline.
test.skip(!process.env.README_SHOTS, 'README_SHOTS is not set');

const OUT = '../../docs/img';

/// A run graph with enough shape to read at a glance: a scoped plan, a fanned-out build and
/// measure, a judge, and a review that has not reported yet.
const RUN_GRAPH = {
  plan_version: 3,
  tasks: [
    { name: 'scope', kind: 'agent', depends_on: [], session: 'plan', needs: 'any', required: true },
    { name: 'plan', kind: 'agent', depends_on: ['scope'], session: 'plan', needs: 'all', required: true },
    { name: 'build', kind: 'command', depends_on: ['plan'], session: '', needs: 'all', required: true },
    { name: 'measure', kind: 'command', depends_on: ['build'], session: '', needs: 'all', required: true },
    { name: 'profile', kind: 'command', depends_on: ['build'], session: '', needs: 'any', required: false },
    { name: 'judge', kind: 'top_k', depends_on: ['measure', 'profile'], session: '', needs: 'all', required: true },
    { name: 'review', kind: 'agent', depends_on: ['judge'], session: 'review', needs: 'all', required: true },
    { name: 'pr', kind: 'command', depends_on: ['review'], session: '', needs: 'all', required: true },
  ],
  results: [
    { iter: 0, task: 'scope', status: 'pass', note: 'three candidate kernels, one ruled out', cost_usd: 0.62, secs: 141 },
    { iter: 0, task: 'plan', status: 'pass', note: 'fanned out over 3 candidates', cost_usd: 1.9, secs: 388 },
    { iter: 0, task: 'build[fused-rmsnorm]', status: 'pass', note: 'image sha-3f1c', cost_usd: null, secs: 212 },
    { iter: 0, task: 'build[paged-swizzle]', status: 'pass', note: 'image sha-8a0e', cost_usd: null, secs: 205 },
    { iter: 0, task: 'build[tiled-gemv]', status: 'fail', note: 'nvcc: identifier "tile_k" is undefined\n  at gemv.cu:88', cost_usd: null, secs: 61 },
    { iter: 0, task: 'measure[fused-rmsnorm]', status: 'pass', note: 'ttft p50 41.2ms (-6.1%)', cost_usd: 0.31, secs: 620 },
    { iter: 0, task: 'measure[paged-swizzle]', status: 'pass', note: 'ttft p50 44.8ms (+2.0%)', cost_usd: 0.31, secs: 598 },
    { iter: 0, task: 'profile[fused-rmsnorm]', status: 'pass', note: 'flame captured', cost_usd: null, secs: 92 },
    { iter: 0, task: 'judge', status: 'pass', note: 'kept 1 of 2', cost_usd: null, secs: 3 },
  ],
};

/// The compiled plan a draft previews: agents on two harnesses, a fan-out, an optional lint,
/// and a gated file step.
const PREVIEW_GRAPH = {
  workflow_type: 'playbook',
  result: 'file',
  nodes: [
    { name: 'read', kind: 'agent', required: true, needs: 'any', join: 'all', isolation: 'worktree', emits: ['paper'], emits_files: [], fanout: null, session: 'survey', harness: 'claude', model: 'opus', effort: 'high', prompt: 'Read the paper and report one entry per citation.\n', command: null },
    { name: 'summarize', kind: 'command', required: true, needs: 'any', join: 'all', isolation: null, emits: [], emits_files: [], fanout: { over_task: 'read', over_field: 'paper', max_fanout: 4 }, session: null, harness: null, model: null, effort: null, prompt: null, command: './summarize.sh --one' },
    { name: 'critique', kind: 'agent', required: true, needs: 'any', join: 'all', isolation: 'worktree', emits: ['verdict'], emits_files: [], fanout: null, session: 'survey', harness: 'codex', model: 'gpt-5', effort: 'medium', prompt: 'Argue against each summary.\n', command: null },
    { name: 'lint', kind: 'command', required: false, needs: 'any', join: 'all', isolation: null, emits: [], emits_files: [], fanout: null, session: null, harness: null, model: null, effort: null, prompt: null, command: './lint.sh' },
    { name: 'rank', kind: 'engine', required: true, needs: 'all', join: 'all', isolation: null, emits: [], emits_files: [], fanout: null, session: null, harness: null, model: null, effort: null, prompt: null, command: null },
    { name: 'file', kind: 'command', required: true, needs: 'all', join: 'passed', isolation: null, emits: [], emits_files: ['SURVEY.md'], fanout: null, session: null, harness: null, model: null, effort: null, prompt: null, command: './file.sh' },
  ],
  edges: [
    { from: 'read', to: 'summarize', join: 'all', required: true },
    { from: 'read', to: 'lint', join: 'all', required: false },
    { from: 'summarize', to: 'critique', join: 'all', required: true },
    { from: 'critique', to: 'rank', join: 'all', required: true },
    { from: 'rank', to: 'file', join: 'passed', required: true },
    { from: 'lint', to: 'file', join: 'passed', required: true },
  ],
};

async function open(page: Page, path: string, theme: 'dark' | 'light'): Promise<void> {
  await page.emulateMedia({ colorScheme: theme });
  await page.addInitScript((t) => {
    localStorage.setItem('theme', t);
  }, theme);
  await stubApi(page);
  await page.route('**/api/runs/RUN-0412/graph', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(RUN_GRAPH) }),
  );
  const preview = ROUTES['/api/playbook-drafts/studio/preview'];
  await page.route('**/api/playbook-drafts/studio/preview', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ ...preview, graph: PREVIEW_GRAPH }) }),
  );
  await page.goto(path);
  await page.waitForLoadState('networkidle');
  await expect(page.locator('main')).toBeVisible();
}

const THEME = 'dark';

test('run graph', async ({ page }) => {
  await page.setViewportSize({ width: 1600, height: 1000 });
  await open(page, '/playbook-runs/playbook%3Asurvey%3A0199', THEME);
  const graph = page.getByTestId('workflow-graph');
  await expect(graph).toBeVisible();
  await expect(graph.locator('[data-task="judge"]')).toBeVisible();
  await page.waitForTimeout(600);
  await page.screenshot({ path: `${OUT}/controller-run.png`, fullPage: false });
  await graph.screenshot({ path: `${OUT}/controller-run-graph.png` });
});

test('draft studio', async ({ page }) => {
  await page.setViewportSize({ width: 1600, height: 1000 });
  await open(page, '/playbooks/drafts/studio', THEME);
  await expect(page.getByTestId('draft-editor').locator('.view-lines')).not.toBeEmpty();
  await expect(page.getByTestId('workflow-graph')).toBeVisible();
  await page.waitForTimeout(800);
  await page.screenshot({ path: `${OUT}/controller-studio.png`, fullPage: false });
});

test('dashboard', async ({ page }) => {
  await page.setViewportSize({ width: 1600, height: 1000 });
  await open(page, '/', THEME);
  await page.waitForTimeout(400);
  await page.screenshot({ path: `${OUT}/controller-dashboard.png`, fullPage: false });
});
