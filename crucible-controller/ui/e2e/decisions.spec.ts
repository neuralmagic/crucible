import { expect, test, type Page, type Route } from '@playwright/test';
import { stubApi } from './api';

const ID = '0199d000-0000-7000-8000-00000000d001';
const DIGEST = 'sha256:5f2c';
const b64 = (text: string) => Buffer.from(text, 'utf8').toString('base64');

const SUMMARY = {
  id: ID,
  run_id: 'playbook_gpu-gate_01a1',
  task: 'gate',
  launch_key: 'playbook:gpu-gate:01a1',
  opened_at: '2026-10-06T12:00:00Z',
  expires_at: '2099-10-06T14:00:00Z',
  questions: ['go'],
};

const DECISION = {
  ...SUMMARY,
  state: 'open',
  questions: [
    { id: 'go', instructions: 'Launch the training job?', kind: 'choice', multiple: false, options: ['approve', 'deny'] },
  ],
  inputs: [
    {
      task: 'plan',
      status: 'pass',
      output: { gpus: 8, est_usd: 412.5 },
      files: [
        {
          path: 'out/chart.html',
          media_type: 'text/html',
          base64: b64('<h1 id="chart">8 × H100</h1><script>document.title = "drawn"</script>'),
        },
        { path: 'out/nodes.csv', media_type: 'text/csv', base64: b64('node,gpus\nn1,4\nn2,4\n') },
      ],
    },
  ],
  spent_usd: 0.12,
  elapsed_secs: 90,
  max_cost_usd: 5,
  max_time_secs: 3600,
  gated: [
    { name: 'launch', kind: 'command', question: 'go', labels: ['approve'] },
    { name: 'shelve', kind: 'command', question: 'go', labels: ['deny', 'uncertain'] },
  ],
  review: 'Launch **8** GPUs for about **$412.50**?',
  evidence_digest: DIGEST,
  answer: null,
  can_answer: true,
};

const json = (route: Route, body: unknown) =>
  route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(body) });

const MULTI = {
  ...DECISION,
  questions: [
    ...DECISION.questions,
    { id: 'checks', instructions: 'Which checks first?', kind: 'choice', multiple: true, options: ['lint', 'unit', 'e2e'] },
    { id: 'nodes', instructions: 'Which nodes?', kind: 'pick', multiple: true, options: ['gpu-a1', 'gpu-b2', 'gpu-c3'] },
  ],
};

async function stubDecisions(
  page: Page,
  open: () => unknown[],
  decision: object = DECISION,
): Promise<{ answers: unknown[] }> {
  const answers: unknown[] = [];
  let answered: unknown = null;
  await page.route('**/api/decisions', (route) => json(route, { open: answered ? [] : open() }));
  await page.route(`**/api/decisions/${ID}`, (route) =>
    json(
      route,
      answered
        ? {
            ...decision,
            state: 'answered',
            can_answer: false,
            answer: { labels: answered, decided_by: 'user:wren', decided_at: '2026-10-06T12:05:00Z', note: 'ship it' },
          }
        : decision,
    ),
  );
  await page.route(`**/api/decisions/${ID}/answer`, (route) => {
    const body: unknown = route.request().postDataJSON();
    answers.push(body);
    answered = typeof body === 'object' && body !== null && 'labels' in body ? body.labels : {};
    return json(route, { ...decision, state: 'answered', can_answer: false });
  });
  return { answers };
}

test.describe('decision requests', () => {
  test('the approvals page lists an open request and links to its evidence', async ({ page }) => {
    await stubApi(page);
    await stubDecisions(page, () => [SUMMARY]);
    await page.goto('/approvals');
    const row = page.getByTestId('decisions-open').getByRole('link');
    await expect(row).toContainText('gate');
    await expect(row).toContainText('playbook:gpu-gate:01a1');
    await row.click();
    await expect(page).toHaveURL(`/decisions/${ID}`);
  });

  test('the evidence page shows the review, outputs, files, and what each answer starts', async ({ page }) => {
    await stubApi(page);
    await stubDecisions(page, () => [SUMMARY]);
    await page.goto(`/decisions/${ID}`);

    await expect(page.getByText('Launch 8 GPUs for about $412.50?')).toBeVisible();
    await expect(page.locator('strong', { hasText: '8' })).toBeVisible();
    await expect(page.getByTestId('decision-output')).toContainText('"est_usd": 412.5');
    await expect(page.getByTestId('decision-gated')).toContainText('go = approve');
    await expect(page.getByTestId('decision-gated')).toContainText('launch');

    const frame = page.locator('iframe[title="out/chart.html"]');
    await expect(frame).toHaveAttribute('sandbox', 'allow-scripts');
    await expect(frame.contentFrame().locator('#chart')).toHaveText('8 × H100');
    const srcdoc = await frame.getAttribute('srcdoc');
    expect(srcdoc).toMatch(/^<meta http-equiv="Content-Security-Policy" content="default-src 'none'/);

    const csv = page.locator('[data-view="csv"] table');
    await expect(csv.locator('th')).toHaveText(['node', 'gpus']);
    await expect(csv.locator('tbody tr')).toHaveCount(2);
  });

  test('answering sends every label with the evidence digest and shows the answer', async ({ page }) => {
    await stubApi(page);
    const { answers } = await stubDecisions(page, () => [SUMMARY]);
    await page.goto(`/decisions/${ID}`);

    const submit = page.getByRole('button', { name: 'Submit' });
    await expect(submit).toBeDisabled();
    const approve = page.getByRole('button', { name: 'approve' });
    const deny = page.getByRole('button', { name: 'deny' });
    await expect(approve).toHaveAttribute('aria-pressed', 'false');
    await approve.click();
    await expect(approve).toHaveAttribute('aria-pressed', 'true');
    await expect(approve).toHaveText('✓ approve');
    await expect(deny).toHaveAttribute('aria-pressed', 'false');
    await deny.click();
    await expect(deny).toHaveAttribute('aria-pressed', 'true');
    await expect(approve).toHaveAttribute('aria-pressed', 'false');
    await approve.click();
    await page.getByLabel('Note').fill('ship it');
    await submit.click();

    await expect(page.getByTestId('decision-answer')).toContainText('✓ approve');
    await expect(page.getByTestId('decision-answer')).toContainText('user:wren');
    expect(answers).toEqual([{ labels: { go: ['approve'] }, evidence_digest: DIGEST, note: 'ship it' }]);
  });

  test('a multiple choice toggles several labels and a pick sends the values in the order chosen', async ({ page }) => {
    await stubApi(page);
    const { answers } = await stubDecisions(page, () => [SUMMARY], MULTI);
    await page.goto(`/decisions/${ID}`);

    const submit = page.getByRole('button', { name: 'Submit' });
    const checks = page.locator('fieldset[data-kind="choice"][data-multiple="true"]');
    const nodes = page.locator('fieldset[data-kind="pick"]');
    await expect(checks.locator('legend')).toContainText('any');
    await expect(nodes.getByRole('button')).toHaveText(['gpu-a1', 'gpu-b2', 'gpu-c3']);
    await expect(nodes.getByRole('button', { name: 'gpu-a1' })).toHaveCSS('text-transform', 'none');

    await page.getByRole('button', { name: 'approve' }).click();
    await checks.getByRole('button', { name: 'unit' }).click();
    await checks.getByRole('button', { name: 'lint' }).click();
    await checks.getByRole('button', { name: 'e2e' }).click();
    await checks.getByRole('button', { name: 'e2e' }).click();
    await expect(checks.getByRole('button', { name: 'unit' })).toHaveAttribute('aria-pressed', 'true');
    await expect(checks.getByRole('button', { name: 'e2e' })).toHaveAttribute('aria-pressed', 'false');
    await expect(submit).toBeDisabled();
    await nodes.getByRole('button', { name: 'gpu-c3' }).click();
    await nodes.getByRole('button', { name: 'gpu-a1' }).click();
    await submit.click();

    expect(answers).toEqual([
      {
        labels: { go: ['approve'], checks: ['unit', 'lint'], nodes: ['gpu-c3', 'gpu-a1'] },
        evidence_digest: DIGEST,
        note: null,
      },
    ]);
    const shown = page.getByTestId('decision-answer');
    await expect(shown).toContainText('✓ unit');
    await expect(shown).toContainText('✓ lint');
    await expect(shown).toContainText('✓ gpu-c3');
  });

  test('a page that throws while rendering shows the error and the rest of the app keeps working', async ({ page }) => {
    await stubApi(page);
    await stubDecisions(page, () => [SUMMARY], {
      ...DECISION,
      questions: [{ id: 'go', instructions: 'Launch the training job?', labels: ['approve', 'deny'] }],
    });
    await page.goto(`/decisions/${ID}`);

    const failed = page.getByTestId('error-boundary');
    await expect(failed).toContainText('Page failed');
    await expect(failed.getByRole('button', { name: 'Reload' })).toBeVisible();
    await page.getByRole('link', { name: 'Approvals' }).first().click();
    await expect(page).toHaveURL('/approvals');
    await expect(failed).toHaveCount(0);
    await expect(page.getByTestId('decisions-open')).toBeVisible();
  });

  test('a request that opens while the app is up raises a notification that opens it', async ({ page }) => {
    await page.addInitScript(() => {
      class FakeNotification {
        static permission = 'granted';
        static requestPermission = () => Promise.resolve('granted');
        onclick: (() => void) | null = null;
        constructor(title: string, options?: NotificationOptions) {
          const raised = Reflect.get(window, '__raised');
          const list: { title: string; body: string | undefined; open: () => void }[] = Array.isArray(raised) ? raised : [];
          list.push({ title, body: options?.body, open: () => this.onclick?.() });
          Reflect.set(window, '__raised', list);
        }
      }
      Reflect.set(window, 'Notification', FakeNotification);
    });
    await page.clock.install();
    await stubApi(page);
    let open: unknown[] = [];
    await stubDecisions(page, () => open);
    await page.goto('/');
    await page.waitForLoadState('networkidle');

    open = [SUMMARY];
    await page.clock.fastForward(31_000);
    await expect
      .poll(() => page.evaluate(() => {
        const raised = Reflect.get(window, '__raised');
        return Array.isArray(raised) ? raised.map((n: { title: string; body?: string }) => `${n.title}: ${n.body ?? ''}`) : [];
      }))
      .toEqual(['Decision needed: gate · playbook:gpu-gate:01a1']);

    await page.evaluate(() => {
      const raised = Reflect.get(window, '__raised');
      const first: unknown = Array.isArray(raised) ? raised[0] : undefined;
      if (typeof first === 'object' && first !== null && 'open' in first && typeof first.open === 'function') {
        first.open();
      }
    });
    await expect(page).toHaveURL(`/decisions/${ID}`);
  });

  test('requests already open when the page loads are not announced', async ({ page }) => {
    await page.addInitScript(() => {
      class FakeNotification {
        static permission = 'granted';
        constructor() {
          Reflect.set(window, '__raised', (Number(Reflect.get(window, '__raised')) || 0) + 1);
        }
      }
      Reflect.set(window, 'Notification', FakeNotification);
    });
    await page.clock.install();
    await stubApi(page);
    await stubDecisions(page, () => [SUMMARY]);
    await page.goto('/');
    await page.waitForLoadState('networkidle');
    await page.clock.fastForward(31_000);
    await page.waitForLoadState('networkidle');
    expect(await page.evaluate(() => Reflect.get(window, '__raised') ?? 0)).toBe(0);
  });

  test('"Notify me" asks for permission once and goes away', async ({ page }) => {
    await page.addInitScript(() => {
      class FakeNotification {
        static permission: NotificationPermission = 'default';
        static requestPermission = () => {
          FakeNotification.permission = 'granted';
          Reflect.set(window, '__asked', true);
          return Promise.resolve('granted');
        };
      }
      Reflect.set(window, 'Notification', FakeNotification);
    });
    await stubApi(page);
    await stubDecisions(page, () => [SUMMARY]);
    await page.goto('/approvals');
    const ask = page.getByTestId('decisions-notify');
    await ask.click();
    await expect(ask).toHaveCount(0);
    expect(await page.evaluate(() => Reflect.get(window, '__asked'))).toBe(true);
  });
});
