import { expect, test, type Locator, type Page } from '@playwright/test';
import { stubApi, TRIAGE_RUN } from './api';

const STUDIO = '/playbooks/drafts/studio';

async function ready(page: Page, path: string): Promise<void> {
  const crashes: string[] = [];
  page.on('pageerror', (e) => crashes.push(String(e)));
  await page.goto(path);
  await page.waitForLoadState('networkidle');
  expect(crashes, `uncaught exceptions on ${path}:\n${crashes.join('\n')}`).toEqual([]);
  await expect(page.locator('main')).toBeVisible();
}

async function widthOf(pane: Locator): Promise<number> {
  const box = await pane.boundingBox();
  expect(box).not.toBeNull();
  return box?.width ?? 0;
}

/// Drag a divider by the given offset and let the layout settle.
async function dragBy(page: Page, separator: Locator, dx: number, dy: number): Promise<void> {
  const box = await separator.boundingBox();
  expect(box).not.toBeNull();
  if (box === null) return;
  const x = box.x + box.width / 2;
  const y = box.y + box.height / 2;
  await page.mouse.move(x, y);
  await page.mouse.down();
  await page.mouse.move(x + dx, y + dy, { steps: 8 });
  await page.mouse.up();
}

test.describe('the studio split', () => {
  /// The divider moves the editor against the previews, the size survives a reload on this device,
  /// and double-clicking the divider puts it back.
  test('drags, remembers, and resets on a double click', async ({ page }) => {
    await stubApi(page);
    await ready(page, STUDIO);

    const source = page.locator('[data-testid="source"]');
    const separator = page.getByRole('separator', { name: 'Resize the editor' });
    const before = await widthOf(source);

    await dragBy(page, separator, 180, 0);
    const dragged = await widthOf(source);
    expect(dragged).toBeGreaterThan(before + 100);

    await ready(page, STUDIO);
    expect(Math.abs((await widthOf(source)) - dragged)).toBeLessThan(4);

    await page.getByRole('separator', { name: 'Resize the editor' }).dblclick();
    expect(Math.abs((await widthOf(source)) - before)).toBeLessThan(4);
  });

  /// The editor is as tall as the pane it sits in rather than a height of its own, so a taller
  /// window is a taller editor.
  test('gives the editor the height of its pane', async ({ page }) => {
    await stubApi(page);
    await page.setViewportSize({ width: 1280, height: 800 });
    await ready(page, STUDIO);

    const editor = page.getByTestId('draft-editor');
    await expect(editor.locator('.monaco-editor')).toBeVisible();
    const short = await editor.boundingBox();

    await page.setViewportSize({ width: 1280, height: 1200 });
    await expect
      .poll(async () => (await editor.boundingBox())?.height ?? 0)
      .toBeGreaterThan((short?.height ?? 0) + 300);

    // Two panes at 1280, and the page itself never scrolls sideways.
    await expect(page.locator('[data-testid="previews"]')).toBeVisible();
    const overflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth
    );
    expect(overflow).toBeLessThanOrEqual(0);
  });
});

test.describe('the sidebar', () => {
  /// Collapsed it is an icon rail: the mark stays, the items keep their labels in tooltips, the
  /// keyboard toggles it, and this device remembers the choice.
  test('collapses to an icon rail and stays that way', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/runs');

    const rail = page.getByTestId('category-rail');
    await expect(rail).toHaveAttribute('data-collapsed', 'false');
    await expect(rail.getByRole('link', { name: 'Issues' })).toContainText('Issues');

    await page.getByRole('button', { name: 'Collapse sidebar' }).click();
    await expect(rail).toHaveAttribute('data-collapsed', 'true');
    const icon = rail.getByRole('link', { name: 'Issues' });
    await expect(icon).toHaveText('IS');
    await icon.hover();
    await expect(page.getByText('Issues · 5', { exact: true })).toBeVisible();

    await ready(page, '/runs');
    await expect(page.getByTestId('category-rail')).toHaveAttribute('data-collapsed', 'true');

    await page.keyboard.press('ControlOrMeta+b');
    await expect(page.getByTestId('category-rail')).toHaveAttribute('data-collapsed', 'false');
  });
});

test.describe('the run graph', () => {
  /// The node panel is a pane of the graph rather than a sheet over it: dragging its divider
  /// widens it, and the plan is refit into whatever the canvas has left.
  test('resizes its node panel and refits the plan', async ({ page }) => {
    await stubApi(page);
    await ready(page, `/runs/${encodeURIComponent(TRIAGE_RUN)}`);

    const graph = page.getByTestId('workflow-graph');
    await graph.locator('[data-task="triage[1027]"]').click();

    const panel = page.locator('[data-testid="panel"]');
    const before = await widthOf(panel);
    const transform = await graph.locator('.react-flow__viewport').getAttribute('style');

    await dragBy(page, page.getByRole('separator', { name: 'Resize the task panel' }), -160, 0);
    expect(await widthOf(panel)).toBeGreaterThan(before + 100);
    await expect
      .poll(async () => graph.locator('.react-flow__viewport').getAttribute('style'))
      .not.toBe(transform);
    await expect(graph.locator('[data-task="triage[1027]"]')).toBeVisible();
  });

  /// The evidence list and the body beside it are their own panes.
  test('splits the evidence list from what it reads', async ({ page }) => {
    await stubApi(page);
    await ready(page, `/runs/${encodeURIComponent(TRIAGE_RUN)}`);

    await page.getByTestId('workflow-graph').locator('[data-task="triage[1027]"]').click();
    const panel = page.getByRole('complementary', { name: 'Task triage[1027]' });
    const list = panel.locator('[data-testid="list"]');
    const separator = panel.getByRole('separator', { name: 'Resize the evidence content' });

    const before = (await list.boundingBox())?.height ?? 0;
    await dragBy(page, separator, 0, 90);
    const dragged = (await list.boundingBox())?.height ?? 0;
    expect(dragged).toBeGreaterThan(before + 50);

    await separator.dblclick();
    await expect.poll(async () => (await list.boundingBox())?.height ?? 0).toBeLessThan(dragged - 40);
    await expect(panel.getByTestId('task-evidence-content')).toBeVisible();
  });
});
