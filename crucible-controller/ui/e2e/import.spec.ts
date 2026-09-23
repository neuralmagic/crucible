import { expect, test, type Page, type Route } from '@playwright/test';
import { IMPORT_ID, PACK_IMPORT, stubApi } from './api';

const IMPORT = '/playbooks/import';

async function ready(page: Page, path: string): Promise<void> {
  const crashes: string[] = [];
  page.on('pageerror', (e) => crashes.push(String(e)));
  await page.goto(path);
  await page.waitForLoadState('networkidle');
  expect(crashes, `uncaught exceptions on ${path}:\n${crashes.join('\n')}`).toEqual([]);
  await expect(page.locator('main')).toBeVisible();
}

/// Serve one import row for both the proposal and the link it lands on. The row's shares stay
/// with the fixture stubs.
async function importWith(page: Page, body: Record<string, unknown>): Promise<void> {
  await page.route('**/api/playbooks/imports**', (route: Route) =>
    new URL(route.request().url()).pathname.endsWith('/shares')
      ? route.fallback()
      : route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(body) }),
  );
}

/// Fetch a repo nothing is registered from and propose a pack, leaving the wizard on that
/// proposal's own page as a fresh import rather than a pin bump.
async function toReview(page: Page): Promise<void> {
  await ready(page, IMPORT);
  await page.locator('#import-repo').fill('neuralmagic/other-packs');
  await page.getByRole('button', { name: 'FETCH PACKS' }).click();
  await page.getByRole('button', { name: 'packs/survey' }).click();
  await expect(page).toHaveURL(new RegExp(`${IMPORT}/`));
}

test.describe('pack import wizard', () => {
  /// A pack that asks for GPUs says so where it is reviewed, before anyone registers it.
  test('the substrate names the sandbox resources the pack asks for', async ({ page }) => {
    await stubApi(page);
    await importWith(page, {
      ...PACK_IMPORT,
      dispatch: { ...PACK_IMPORT.dispatch, resources: { gpus: 1, cpu: null, memory: '32Gi', node_selector: {} } },
    });
    await toReview(page);

    await expect(page.getByText('1 GPU · memory 32Gi')).toBeVisible();
  });

  test('lists the candidate packs at a ref', async ({ page }) => {
    await stubApi(page);
    await ready(page, IMPORT);

    await page.locator('#import-repo').fill('neuralmagic/crucible-packs');
    await page.locator('#import-ref').fill('main');
    await page.getByRole('button', { name: 'FETCH PACKS' }).click();

    await expect(page.getByRole('button', { name: 'packs/survey workflow.star' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'packs/audit graph.star' })).toBeVisible();
  });

  /// The gate is the point of the wizard: the form, a real rendered graph, and no registration
  /// until someone has looked at both.
  test('the preview gate renders the form and the graph before anything registers', async ({
    page,
  }) => {
    await stubApi(page);
    await toReview(page);

    await expect(page.locator('#preview-param-topic')).toBeVisible();
    await expect(page.locator('#preview-param-depth')).toHaveValue('deep');
    await expect(page.getByText('What the survey covers.')).toBeVisible();

    const graph = page.getByTestId('workflow-graph');
    await expect(graph).toBeVisible();
    await expect(graph.locator('.react-flow__edges svg path.react-flow__edge-path').first()).toBeVisible();
    const read = graph.locator('[data-task="read"]');
    const file = graph.locator('[data-task="file"]');
    await expect(read).toBeVisible();
    await expect(file).toBeVisible();
    // What each card owes a reader: the knobs an agent turn runs with, the command a task runs,
    // what a mapped task maps over, the join on the edge that waits for it, and the advisory task
    // drawn weaker.
    await expect(read).toContainText('opus · high');
    await expect(graph.locator('[data-task="summarize"]')).toContainText('./summarize.sh --one');
    await expect(graph.locator('[data-task="summarize"]')).toContainText('over read.paper ≤3');
    await expect(file).toContainText('spec.md');
    await expect(graph.getByText('passed', { exact: true }).first()).toBeVisible();
    await expect(graph.locator('[data-task="lint"]')).toHaveCSS('border-style', 'dashed');
    // Ancestors light up on hover; everything off that path is dimmed rather than hidden.
    await file.hover();
    await expect(read).toHaveCSS('opacity', '1');
    await expect(graph.locator('[data-task="lint"]')).toHaveCSS('opacity', '1');
    await page.mouse.move(0, 0);
    await read.hover();
    await expect(file).not.toHaveCSS('opacity', '1');

    await expect(page.getByRole('button', { name: 'REGISTER' })).toBeDisabled();
    await page.locator('#import-id').fill('survey-two');
    await page.locator('#import-description').fill('reads a paper and files a spec');
    await expect(page.getByRole('button', { name: 'REGISTER' })).toBeEnabled();
  });

  /// What the pack's agent needs is read at the gate, not discovered by a failed run: the review
  /// names the backend and the sandbox image, and says this deployment cannot dispatch them.
  test('the gate names the substrate and warns when this deployment cannot dispatch it', async ({
    page,
  }) => {
    await stubApi(page);
    await toReview(page);

    await expect(page.getByText('Substrate')).toBeVisible();
    await expect(page.getByText('[agent] backend = openshell')).toBeVisible();
    await expect(page.getByText('ghcr.io/example/sandbox:latest', { exact: true })).toBeVisible();
    await expect(page.getByText('Cannot dispatch')).toBeVisible();
    await expect(
      page.getByText('needs an OpenShell sandbox on a cluster', { exact: false }),
    ).toBeVisible();
  });

  /// The image preflight is read at the gate too: an image the catalog cannot vouch for is named
  /// with the way out, and the standing line says where it stands.
  test('the gate shows the image preflight refusal', async ({ page }) => {
    await stubApi(page);
    await toReview(page);

    await expect(page.getByText('Image preflight')).toBeVisible();
    await expect(page.getByText('is not in the image catalog', { exact: false })).toBeVisible();
  });

  /// What a scope has to bind before this pack can launch, read at the gate rather than
  /// discovered by a refused launch.
  test('the gate names the credentials the pack declares', async ({ page }) => {
    await stubApi(page);
    await toReview(page);

    await expect(page.getByText('pr_token (opaque) \u2192 env AUTORESEARCH_PR_TOKEN')).toBeVisible();
    await expect(
      page.getByText('registry (registry_authfile) \u2192 file /etc/quay/push.json'),
    ).toBeVisible();
    await expect(page.getByText('until the scope it runs under binds every one', { exact: false })).toBeVisible();
  });

  /// The proposal is a durable row, so its URL is the artifact: hand it to someone (or just
  /// reload) and the same frozen preview comes back with no wizard state behind it.
  test('a proposal has its own link that a fresh page rehydrates', async ({ page }) => {
    await stubApi(page);
    await toReview(page);
    const link = page.url();
    expect(link).toContain(`${IMPORT}/${IMPORT_ID}`);

    await ready(page, new URL(link).pathname);

    await expect(page.getByTestId('import-rev')).toContainText('4d5e6f708192');
    await expect(page.getByTestId('import-status')).toHaveText('pending');
    await expect(page.getByText('authoring-agent')).toBeVisible();
    await expect(page.locator('#preview-param-topic')).toBeVisible();
    await expect(page.getByTestId('workflow-graph')).toBeVisible();
    await expect(page.getByRole('button', { name: 'DISCARD' })).toBeEnabled();
    await expect(page.getByRole('button', { name: 'OPEN AS DRAFT' })).toBeDisabled();
    await page.locator('#import-draft-id').fill('survey-draft');
    await expect(page.getByRole('button', { name: 'OPEN AS DRAFT' })).toBeEnabled();
  });

  /// What crucible_playbook_import hands a human: the proposal's link carrying the registry
  /// naming the agent chose, so the admin reviews a filled form rather than retyping it.
  test('a link carrying the proposed naming opens the register form on it', async ({ page }) => {
    await stubApi(page);
    await ready(page, `${IMPORT}/${IMPORT_ID}?id=survey-two&description=reads%20a%20paper`);

    await expect(page.locator('#import-id')).toHaveValue('survey-two');
    await expect(page.locator('#import-description')).toHaveValue('reads a paper');
    await expect(page.getByRole('button', { name: 'REGISTER' })).toBeEnabled();
  });

  test('a pack that does not compile shows the engine error and offers no registration', async ({
    page,
  }) => {
    await stubApi(page);
    await importWith(page, {
      id: IMPORT_ID,
      repo: 'neuralmagic/other-packs',
      git_ref: null,
      path: 'packs/survey',
      rev: '4d5e6f708192a3b4c5d6e7f80912a3b4c5d6e7f8',
      tar_digest: 'sha256:5555',
      params_schema: null,
      schema_digest: null,
      graph: null,
      diagnostics: ['workflow.star:3:5: unknown identifier `dpeth`'],
      dispatch: {
        backend: 'local',
        sandbox_image: null,
        harness: null,
        requires: {},
        prefers: {},
        allow_unverified_image: false,
        resources: { gpus: 0, cpu: null, memory: null, node_selector: {} },
        image: {
          reference: null,
          digest: null,
          capability_digest: null,
          tags: [],
          checked: false,
          catalogued: false,
          verified: false,
          overridden: false,
          unsatisfied: [],
          refusals: [],
          warnings: [],
        },
        dispatchable: true,
        refusal: null,
        local_mode: true,
      },
      secrets: { declared: [], warnings: [] },
      exposure: { document: null, digest: 'sha256:expo', approved_digest: null, lines: [] },
      core_rev: '7c2c1a5',
      status: 'pending',
      playbook: null,
      draft_id: null,
      proposed_by: 'authoring-agent',
      created_at: '2026-08-23T09:00:00Z',
      resolved_by: null,
      resolved_at: null,
    });
    await toReview(page);

    await expect(page.getByText('workflow.star:3:5: unknown identifier `dpeth`')).toBeVisible();
    await expect(page.getByText('NOTHING TO REGISTER')).toBeVisible();
    await expect(page.getByRole('button', { name: 'REGISTER' })).toHaveCount(0);
    await expect(page.locator('#import-id')).toHaveCount(0);
  });

  /// A proposal that already registered is the record of what happened, not a control surface.
  test('a resolved import offers nothing to act on', async ({ page }) => {
    await stubApi(page);
    await importWith(page, {
      id: IMPORT_ID,
      repo: 'neuralmagic/other-packs',
      git_ref: null,
      path: 'packs/survey',
      rev: '4d5e6f708192a3b4c5d6e7f80912a3b4c5d6e7f8',
      tar_digest: 'sha256:5555',
      params_schema: { type: 'object', properties: {}, required: [] },
      schema_digest: 'sha256:3333',
      graph: null,
      diagnostics: [],
      dispatch: {
        backend: 'local',
        sandbox_image: null,
        harness: null,
        requires: {},
        prefers: {},
        allow_unverified_image: false,
        resources: { gpus: 0, cpu: null, memory: null, node_selector: {} },
        image: {
          reference: null,
          digest: null,
          capability_digest: null,
          tags: [],
          checked: false,
          catalogued: false,
          verified: false,
          overridden: false,
          unsatisfied: [],
          refusals: [],
          warnings: [],
        },
        dispatchable: true,
        refusal: null,
        local_mode: true,
      },
      secrets: { declared: [], warnings: [] },
      exposure: { document: null, digest: 'sha256:expo', approved_digest: null, lines: [] },
      core_rev: '7c2c1a5',
      status: 'registered',
      playbook: 'survey-two',
      draft_id: null,
      proposed_by: 'authoring-agent',
      created_at: '2026-08-23T09:00:00Z',
      resolved_by: 'wren',
      resolved_at: '2026-08-23T10:00:00Z',
    });
    await ready(page, `${IMPORT}/${IMPORT_ID}`);

    await expect(page.getByTestId('import-status')).toHaveText('registered');
    await expect(page.getByText('registered as survey-two')).toBeVisible();
    await expect(page.getByRole('button', { name: 'REGISTER' })).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'DISCARD' })).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'OPEN AS DRAFT' })).toHaveCount(0);
  });

  /// Re-importing a pack already in the registry is a pin bump, and the form change is shown
  /// before it is accepted.
  test('a re-import shows the schema diff and re-pins', async ({ page }) => {
    await stubApi(page);
    await page.route('**/api/playbooks/import/candidates', (route: Route) =>
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          rev: '4d5e6f708192a3b4c5d6e7f80912a3b4c5d6e7f8',
          candidates: [{ path: 'packs/survey', workflow_file: 'workflow.star' }],
        }),
      }),
    );

    await ready(page, IMPORT);
    await page.locator('#import-repo').fill('neuralmagic/crucible-packs');
    await page.getByRole('button', { name: 'FETCH PACKS' }).click();
    await expect(page).toHaveURL(new RegExp(`${IMPORT}/`));

    await expect(page.getByText('Already registered')).toBeVisible();
    await expect(page.getByText('~ depth.default: shallow → deep')).toBeVisible();
    await expect(page.locator('#import-id')).toHaveValue('survey');
    await expect(page.getByRole('button', { name: 'RE-PIN' })).toBeEnabled();
  });

  /// Pending proposals sit on the approvals rail beside the scope packs, with whoever proposed
  /// them and a link straight to the gate.
  test('a pending import waits on the approvals rail', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/approvals');

    const row = page.getByRole('row').filter({ hasText: 'authoring-agent' });
    await expect(row).toContainText('neuralmagic/other-packs · packs/survey');
    await expect(row).toContainText('4d5e6f708192');
    await row.getByRole('link').click();
    await expect(page).toHaveURL(new RegExp(`${IMPORT}/${IMPORT_ID}`));
    await expect(page.getByTestId('import-status')).toHaveText('pending');
  });

  /// Clicking a task opens everything the graph document holds about it, including the prompt the
  /// card has no room for.
  test('picking a task opens its metadata and Escape closes it', async ({ page }) => {
    await stubApi(page);
    await toReview(page);

    const graph = page.getByTestId('workflow-graph');
    await graph.locator('[data-task="read"]').click();

    const panel = page.getByRole('complementary', { name: 'Task read' });
    await expect(panel).toBeVisible();
    await expect(panel).toContainText('worktree');
    await expect(panel).toContainText('survey');
    await expect(panel).toContainText('claude');
    await expect(panel).toContainText('opus');
    await expect(panel).toContainText('high');
    await expect(panel).toContainText('paper');
    await expect(panel).toContainText('READ THE PAPER');

    await page.keyboard.press('Escape');
    await expect(panel).toBeHidden();

    // A mapped task says what it maps over and how wide it may get.
    await graph.locator('[data-task="summarize"]').click();
    const mapped = page.getByRole('complementary', { name: 'Task summarize' });
    await expect(mapped).toContainText('read.paper');
    await expect(mapped).toContainText('3');
    await expect(mapped).toContainText('./summarize.sh --one');
    await graph.locator('.react-flow__pane').click({ position: { x: 4, y: 4 } });
    await expect(mapped).toBeHidden();
  });

  /// A pack names its own tasks, so a name that is markup has to land as text. Every label the
  /// graph draws is a React text node, which is what makes that true by construction.
  test('a task named with markup renders as inert text', async ({ page }) => {
    await stubApi(page);
    const injected: string[] = [];
    page.on('dialog', (dialog) => {
      injected.push(dialog.message());
      void dialog.dismiss();
    });
    await toReview(page);

    const graph = page.getByTestId('workflow-graph');
    await expect(graph.locator('.react-flow__edges svg path.react-flow__edge-path').first()).toBeVisible();
    const hostile = graph.getByRole('button', { name: 'ENGINE <img src=x onerror="alert(1)">' });
    await expect(hostile).toBeVisible();
    await expect(hostile.locator('span[title]')).toHaveText('<img src=x onerror="alert(1)">');
    expect(await graph.locator('img').count(), 'no element was parsed out of the name').toBe(0);
    expect(injected, 'nothing in a task name executed').toEqual([]);
  });
});
