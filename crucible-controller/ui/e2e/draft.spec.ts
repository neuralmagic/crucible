import { expect, test, type Locator, type Page, type Route } from '@playwright/test';
import { ROUTES, stubApi } from './api';

const STUDIO = '/playbooks/drafts/studio';

async function ready(page: Page, path: string): Promise<void> {
  const crashes: string[] = [];
  page.on('pageerror', (e) => crashes.push(String(e)));
  await page.goto(path);
  await page.waitForLoadState('networkidle');
  expect(crashes, `uncaught exceptions on ${path}:\n${crashes.join('\n')}`).toEqual([]);
  await expect(page.locator('main')).toBeVisible();
}

/// The studio's Monaco surface, once its chunk has loaded and it has painted a line.
async function editor(page: Page): Promise<Locator> {
  const surface = page.getByTestId('draft-editor');
  await expect(surface.locator('.monaco-editor')).toBeVisible();
  await expect(surface.locator('.view-lines')).not.toBeEmpty();
  return surface;
}

/// Replace the buffer. `insertText` rather than `type`: it delivers the text as one input event,
/// so Monaco's auto-closing and auto-indent never rewrite what the test meant to write.
async function retype(page: Page, text: string): Promise<void> {
  const surface = await editor(page);
  await surface.locator('.view-lines').click();
  await page.keyboard.press('ControlOrMeta+A');
  await page.keyboard.insertText(text);
}

/// Pick a file out of the tree panel by its name in the tree.
function treeItem(page: Page, name: string): Locator {
  return page.getByTestId('draft-file-tree').getByRole('treeitem', { name, exact: true });
}

const REFUSED = {
  version: 2,
  saved_by: 'wren',
  saved_at: '2026-08-22T09:05:00Z',
  params_schema: null,
  schema_digest: null,
  graph: null,
  diagnostics: [
    {
      file: 'workflow.star',
      line: 2,
      col: 8,
      message: 'workflow.star:2:8: unknown identifier `agnet`',
    },
    { file: null, line: null, col: null, message: 'the pack declares no [workflow] table' },
  ],
};

const RECOMPILED = {
  version: 3,
  saved_by: 'wren',
  saved_at: '2026-08-22T09:06:00Z',
  params_schema: {
    type: 'object',
    properties: { subject: { type: 'string', description: 'What to read.' } },
    required: ['subject'],
  },
  schema_digest: 'sha256:9999',
  graph: {
    workflow_type: 'playbook',
    result: 'settle',
    nodes: [
      {
        name: 'settle',
        kind: 'command',
        required: true,
        needs: 'any',
        join: 'all',
        isolation: null,
        emits: [],
        emits_files: [],
        fanout: null,
        session: null,
        harness: null,
        model: null,
        effort: null,
        prompt: null,
        command: './settle.sh',
      },
    ],
    edges: [],
  },
  diagnostics: [],
};

const AGENT_SAVE = {
  version: 2,
  saved_by: 'agent:author',
  saved_at: '2026-08-23T10:00:00Z',
  diagnostics: [],
  files: {
    'crucible.toml': '[workflow]\n',
    'workflow.star': "params = {}\n# the agent's edit\n",
  },
};

/// Answer the next save with a fixed compile result: what the studio renders after a save is
/// entirely that one response.
async function savesWith(page: Page, body: Record<string, unknown>): Promise<void> {
  await page.route('**/api/playbook-drafts/studio/versions', (route: Route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(body) }),
  );
}

test.describe('draft authoring studio', () => {
  test('lists drafts on their own rail, separate from the registry', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/playbooks/drafts');

    await expect(page.getByRole('link', { name: 'studio' })).toBeVisible();
    await expect(page.getByRole('link', { name: 'survey' })).toHaveCount(0);
    await page.locator('#draft-source').selectOption('template');
    await expect(page.locator('#draft-template')).toContainText('survey');
  });

  /// The hint is the front door to the co-draft loop, so it carries this deployment's own
  /// commands, and dismissing it sticks for this browser.
  test('the co-draft hint carries the deployment commands and stays dismissed', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/playbooks/drafts');

    await expect(page.getByText('export CONTROLLER_API_TOKEN=crk_...')).toBeVisible();
    await expect(page.getByRole('link', { name: 'DOWNLOAD SKILL' })).toHaveAttribute(
      'href',
      'https://crucible.example.com/api/playbooks/drafts/skill',
    );

    await page.getByRole('button', { name: 'DISMISS' }).click();
    await expect(page.getByText('Co-draft with your own agent')).toHaveCount(0);

    await ready(page, '/playbooks/drafts');
    await expect(page.getByText('Co-draft with your own agent')).toHaveCount(0);
  });

  /// The one-motion entry: a repo, a ref and a path go in and the studio opens on the draft. The
  /// controller mints the import behind it; the page just has to ask for the right thing.
  test('a draft is opened straight from a git repo', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/playbooks/drafts');

    const posted: string[] = [];
    await page.route('**/api/playbook-drafts/from-git', (route: Route) => {
      posted.push(route.request().postData() ?? '');
      return route.fulfill({
        status: 201,
        contentType: 'application/json',
        body: JSON.stringify({
          import_id: '0192f4a1-2b3c-7d4e-8f90-1a2b3c4d5e6f',
          repo: 'neuralmagic/other-packs',
          git_ref: 'main',
          path: 'packs/survey',
          rev: '4d5e6f708192a3b4c5d6e7f80912a3b4c5d6e7f8',
          draft: { version: 1, saved_by: 'wren', saved_at: '2026-08-23T10:00:00Z', diagnostics: [] },
        }),
      });
    });

    await page.locator('#draft-id').fill('pulled');
    await page.locator('#draft-description').fill('a pack pulled in to edit');
    await page.locator('#draft-source').selectOption('git');
    await expect(page.locator('#draft-template')).toHaveCount(0);
    await expect(
      page.getByRole('button', { name: 'CREATE DRAFT' }),
      'a repo is required before anything is fetched',
    ).toBeDisabled();

    await page.locator('#draft-repo').fill('neuralmagic/other-packs');
    await page.locator('#draft-git-ref').fill('main');
    await page.locator('#draft-path').fill('packs/survey');
    await page.getByRole('button', { name: 'CREATE DRAFT' }).click();

    await expect(page).toHaveURL(/\/playbooks\/drafts\/pulled$/);
    expect(posted).toHaveLength(1);
    expect(JSON.parse(posted[0]) as Record<string, unknown>).toEqual({
      id: 'pulled',
      description: 'a pack pulled in to edit',
      repo: 'neuralmagic/other-packs',
      git_ref: 'main',
      path: 'packs/survey',
      owner: 'user:wren',
    });
  });

  /// The loop the studio exists for: edit, save, see the engine's complaint marked on the line that
  /// produced it, fix it, and watch both previews re-render from that one response.
  test('edit, save, markers, then the form and graph re-render', async ({ page }) => {
    await stubApi(page);
    await ready(page, STUDIO);

    // The first paint is the last save: the pack's files, its form, and its compiled graph.
    await expect(await editor(page)).toContainText('[workflow]');
    await treeItem(page, 'workflow.star').click();
    await expect(await editor(page)).toContainText('READ THE PAPER');
    await expect(page.locator('#studio-param-topic')).toBeVisible();
    await expect(page.getByTestId('workflow-graph').locator('[data-task="read"]')).toBeVisible();

    await retype(page, 'params = {}\nagnet(name = "read")\n');
    await expect(page.getByText('unsaved')).toBeVisible();

    await savesWith(page, REFUSED);
    await page.getByRole('button', { name: 'SAVE' }).click();

    const diagnostics = page.getByTestId('draft-diagnostics');
    await expect(diagnostics).toContainText('workflow.star:2:8');
    await expect(diagnostics).toContainText('unknown identifier `agnet`');
    await expect(diagnostics).toContainText('the pack declares no [workflow] table');
    await expect(page.getByText('NO GRAPH')).toBeVisible();

    // The anchored one is a Monaco marker on its own line, not just a list entry.
    await expect(page.getByTestId('draft-editor').locator('.squiggly-error').first()).toBeVisible();

    // And it navigates: it opens its file and puts the caret on its position.
    await treeItem(page, 'SKILL.md').click();
    await expect(await editor(page)).toContainText('Read the paper');
    await diagnostics.getByRole('button').first().click();
    await expect(treeItem(page, 'workflow.star')).toHaveAttribute('aria-selected', 'true');
    await expect(page.getByTestId('draft-editor').locator('.native-edit-context')).toBeFocused();
    // An unanchored one has no line to go to, so it offers no navigation.
    await expect(diagnostics.getByRole('button').nth(1)).toBeDisabled();

    await savesWith(page, RECOMPILED);
    await retype(page, 'params = {}\nsettle = command(name = "settle", run = "./settle.sh")\n');
    await page.getByRole('button', { name: 'SAVE' }).click();

    await expect(page.getByText('saved · v3')).toBeVisible();
    await expect(page.getByText('The engine compiled this save without complaint.')).toBeVisible();
    await expect(page.getByTestId('draft-editor').locator('.squiggly-error')).toHaveCount(0);
    // Both previews come from that one response: the new form field and the new graph.
    await expect(page.locator('#studio-param-subject')).toBeVisible();
    await expect(page.locator('#studio-param-topic')).toHaveCount(0);
    const settle = page.getByTestId('workflow-graph').locator('[data-task="settle"]');
    await expect(settle).toBeVisible();
    await expect(settle).toContainText('./settle.sh');
    await expect(page.getByTestId('workflow-graph').locator('[data-task="read"]')).toHaveCount(0);
  });

  /// A long chain is unreadable in a third of the page, so the graph can take the whole screen, and
  /// Escape gives it back.
  test('the graph expands to the whole screen and Escape restores it', async ({ page }) => {
    await stubApi(page);
    await ready(page, STUDIO);
    const graph = page.getByTestId('workflow-graph');
    await expect(graph.locator('[data-task="read"]')).toBeVisible();
    await expect(graph).not.toHaveAttribute('data-fullscreen', 'true');

    await graph.getByRole('button', { name: 'Full screen' }).click();
    await expect(graph).toHaveAttribute('data-fullscreen', 'true');
    const box = await graph.boundingBox();
    const viewport = page.viewportSize();
    expect(box?.width).toBe(viewport?.width);
    await expect(graph.getByRole('button', { name: 'Exit full screen' })).toBeVisible();

    await graph.locator('[data-task="read"]').focus();
    await page.keyboard.press('Escape');
    await expect(graph).not.toHaveAttribute('data-fullscreen', 'true');
    await expect(graph.getByRole('button', { name: 'Full screen' })).toBeVisible();
  });

  /// A draft says what it came from, and when that pack re-pinned underneath it the studio offers
  /// the rebase as a real diff of the origin against the buffer — not a sentence about one.
  test('the origin is named, and a moved origin offers a rebase diff', async ({ page }) => {
    await stubApi(page);
    await page.route('**/api/playbook-drafts/studio', (route: Route) => {
      if (route.request().method() !== 'GET') return route.fallback();
      return route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          id: 'studio',
          description: 'A pack being authored in the controller.',
          origin: {
            kind: 'playbook',
            playbook: 'survey',
            import_id: null,
            repo: 'neuralmagic/packs',
            path: 'packs/survey',
            rev: '4d5e6f708192a3b4c5d6e7f80912a3b4c5d6e7f8',
            current_rev: 'aabbccddeeff00112233445566778899aabbccdd',
            moved: true,
          },
          graduation_repo: null,
          graduation_path: null,
          graduation_pr_url: null,
          retired_at: null,
          latest_version: 1,
          compiles: true,
          diagnostics: 0,
          actions: ['read', 'create', 'update', 'delete', 'transfer', 'share', 'launch', 'approve'],
          created_by: 'wren',
          created_at: '2026-08-22T09:00:00Z',
          updated_at: '2026-08-22T09:00:00Z',
          versions: [],
        }),
      });
    });
    await page.route('**/api/playbook-drafts/studio/origin/files', (route: Route) =>
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          kind: 'playbook',
          reference: 'survey',
          rev: 'aabbccddeeff00112233445566778899aabbccdd',
          files: {
            'crucible.toml': '[workflow]\n# re-pinned upstream\n',
            'workflow.star': 'params = {}\n',
          },
        }),
      }),
    );
    await ready(page, STUDIO);

    await expect(page.getByTestId('draft-origin')).toHaveText('from survey@4d5e6f70');

    const rebase = page.getByTestId('draft-rebase');
    await expect(rebase).toContainText('re-pinned to aabbccdd');
    await expect(rebase).toContainText('4d5e6f70');
    const diff = page.getByTestId('draft-rebase-diff');
    await expect(diff.locator('.monaco-diff-editor')).toBeVisible();
    await expect(diff).toContainText('# re-pinned upstream');

    // Graduation opens on where that pack lives, rather than asking for it again.
    await expect(page.locator('#studio-grad-repo')).toHaveValue('neuralmagic/packs');
    await expect(page.locator('#studio-grad-path')).toHaveValue('packs/survey');

    // And any save is downloadable as the pack it is.
    await expect(page.getByTestId('draft-tarball')).toHaveAttribute(
      'href',
      '/api/playbook-drafts/studio/tarball',
    );
  });

  /// A draft based on nothing offers no rebase.
  test('a draft with no origin has nothing to rebase onto', async ({ page }) => {
    await stubApi(page);
    await ready(page, STUDIO);
    await expect(page.getByTestId('draft-origin')).toHaveText('from —');
    await expect(page.getByTestId('draft-rebase')).toHaveCount(0);
  });

  /// The pack nests, so the panel that picks a file nests with it, and a file typed into a
  /// directory implies whatever directories it names.
  test('the file tree nests a pack and a new file joins it where it was typed', async ({ page }) => {
    await stubApi(page);
    await ready(page, STUDIO);

    const tree = page.getByTestId('draft-file-tree');
    await expect(tree).toContainText('skills/');
    await expect(tree).toContainText('read/');
    await expect(treeItem(page, 'crucible.toml')).toHaveAttribute('aria-selected', 'true');

    await page.getByRole('button', { name: 'New file in skills/', exact: true }).click();
    const typed = page.getByLabel('New file path');
    await typed.fill('write/SKILL.md');
    await typed.press('Enter');

    await expect(tree).toContainText('write/');
    await expect(treeItem(page, 'SKILL.md').nth(1)).toHaveAttribute('aria-selected', 'true');
    await expect(page.getByText('unsaved')).toBeVisible();
  });

  /// Renaming and deleting happen on the tree itself, and a directory is a prefix: renaming one
  /// carries every file under it.
  test('the tree renames and deletes what it lists', async ({ page }) => {
    await stubApi(page);
    await ready(page, STUDIO);

    const tree = page.getByTestId('draft-file-tree');
    await page.getByRole('button', { name: 'Rename skills/', exact: true }).click();
    const rename = page.getByLabel('Rename skills/', { exact: true });
    await rename.fill('abilities');
    await rename.press('Enter');

    await expect(tree).toContainText('abilities/');
    await expect(tree).not.toContainText('skills/');
    await treeItem(page, 'SKILL.md').click();
    await expect(await editor(page)).toContainText('Read the paper');
    await expect(page.getByText('unsaved')).toBeVisible();

    // The open file survives its own rename: the buffer follows the path.
    await page.getByRole('button', { name: 'Rename abilities/read/SKILL.md' }).click();
    const retitle = page.getByLabel('Rename abilities/read/SKILL.md');
    await retitle.fill('NOTES.md');
    await retitle.press('Enter');
    await expect(treeItem(page, 'NOTES.md')).toHaveAttribute('aria-selected', 'true');
    await expect(await editor(page)).toContainText('Read the paper');

    // Deleting a directory takes everything under it, and the studio opens on what is left.
    await page.getByRole('button', { name: 'Delete abilities/', exact: true }).click();
    await expect(tree).not.toContainText('NOTES.md');
    await expect(treeItem(page, 'crucible.toml')).toHaveAttribute('aria-selected', 'true');
  });

  /// The graph is the shared component, so the pack's own task names land as text here too.
  test('a task named with markup renders as inert text and its panel reads back', async ({
    page,
  }) => {
    await stubApi(page);
    const injected: string[] = [];
    page.on('dialog', (dialog) => {
      injected.push(dialog.message());
      void dialog.dismiss();
    });
    await ready(page, STUDIO);

    const graph = page.getByTestId('workflow-graph');
    const hostile = graph.getByRole('button', { name: 'ENGINE <img src=x onerror="alert(1)">' });
    await expect(hostile).toBeVisible();
    expect(await graph.locator('img').count(), 'no element was parsed out of the name').toBe(0);
    expect(injected, 'nothing in a task name executed').toEqual([]);

    await graph.locator('[data-task="read"]').click();
    const panel = page.getByRole('complementary', { name: 'Task read' });
    await expect(panel).toContainText('opus');
    await expect(panel).toContainText('READ THE PAPER');
    await page.keyboard.press('Escape');
    await expect(panel).toBeHidden();
  });

  /// Two editors on one draft: the agent's save landed first, so this one is refused with the
  /// version that overtook it rather than clobbering it. The buffer survives the refusal, the
  /// prompt diffs the two, and reloading takes the version that won.
  test('a save from a stale base renders a merge diff instead of clobbering', async ({ page }) => {
    await stubApi(page);
    await ready(page, STUDIO);

    await page.route('**/api/playbook-drafts/studio/versions', (route: Route) =>
      route.fulfill({
        status: 409,
        contentType: 'application/json',
        body: JSON.stringify({
          error: 'this save edited version 1, but agent:author saved version 2',
          base_version: 1,
          current_version: 2,
          saved_by: 'agent:author',
          saved_at: '2026-08-23T10:00:00Z',
        }),
      }),
    );
    await page.route('**/api/playbook-drafts/studio/files*', (route: Route) =>
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(AGENT_SAVE),
      }),
    );

    await treeItem(page, 'workflow.star').click();
    await retype(page, 'params = {}\n# my own edit\n');
    await page.getByRole('button', { name: 'SAVE' }).click();

    const prompt = page.getByTestId('draft-merge-prompt');
    await expect(prompt).toContainText('agent:author');
    await expect(prompt).toContainText('v2');
    await expect(prompt).toContainText('2026-08-23T10:00:00Z');

    // The refusal is a real diff of the two versions, not a sentence about them.
    const diff = page.getByTestId('draft-merge-diff');
    await expect(diff.locator('.monaco-diff-editor')).toBeVisible();
    await expect(diff).toContainText("# the agent's edit");
    await expect(diff).toContainText('# my own edit');

    // Nothing was overwritten: the buffer is still the writer's own text.
    await expect(await editor(page)).toContainText('# my own edit');
    await expect(page.getByText('unsaved')).toBeVisible();

    // Reloading is the only thing that replaces the buffer, and it takes the version that won.
    await page.getByRole('button', { name: /RELOAD V2/ }).click();
    await expect(prompt).toHaveCount(0);
    await treeItem(page, 'workflow.star').click();
    await expect(await editor(page)).toContainText("# the agent's edit");
  });

  /// A launch is refused until what is on screen is what was compiled.
  test('an unsaved buffer cannot be test-fired', async ({ page }) => {
    await stubApi(page);
    await ready(page, STUDIO);

    await expect(page.getByRole('button', { name: 'LAUNCH DRAFT' })).toBeEnabled();
    await treeItem(page, 'workflow.star').click();
    await retype(page, 'params = {}\n');
    await expect(page.getByRole('button', { name: 'LAUNCH DRAFT' })).toBeDisabled();
    await expect(page.getByText('save first')).toBeVisible();
  });

  /// The controls follow what the policy lets this caller do with this draft, not a platform role.
  test('a team member who may only launch gets a read-only studio that still test-fires', async ({
    page,
  }) => {
    await stubApi(page);
    const studio = ROUTES['/api/playbook-drafts/studio'];
    if (typeof studio !== 'object' || studio === null || Array.isArray(studio)) {
      throw new Error('the studio fixture is an object');
    }
    await page.route('**/api/playbook-drafts/studio', (route: Route) => {
      if (route.request().method() !== 'GET') return route.fallback();
      return route.fulfill({
        json: { ...studio, owner: 'team:llm-d', actions: ['read', 'launch'] },
      });
    });
    await ready(page, STUDIO);

    await expect(page.getByRole('button', { name: 'SAVE', exact: true })).toBeDisabled();
    await expect(page.getByRole('button', { name: 'DELETE DRAFT' })).toBeDisabled();
    await expect(page.getByRole('button', { name: 'LAUNCH DRAFT' })).toBeEnabled();
  });

  /// Deleting takes every version with it, so it asks first and names what it is about to drop.
  test('a draft is deleted from the studio behind a confirm that names it', async ({ page }) => {
    await stubApi(page);
    await ready(page, STUDIO);

    const deletes: string[] = [];
    await page.route('**/api/playbook-drafts/studio', (route: Route) => {
      if (route.request().method() !== 'DELETE') return route.fallback();
      deletes.push(route.request().url());
      return route.fulfill({ status: 204, body: '' });
    });

    await page.getByRole('button', { name: 'DELETE DRAFT' }).click();
    const confirm = page.getByTestId('draft-delete-confirm');
    await expect(confirm).toContainText('Delete draft studio?');

    await confirm.getByRole('button', { name: 'CANCEL' }).click();
    await expect(confirm).toHaveCount(0);
    expect(deletes, 'cancelling deleted nothing').toEqual([]);

    await page.getByRole('button', { name: 'DELETE DRAFT' }).click();
    await page.getByTestId('draft-delete-confirm').getByRole('button', { name: 'DELETE' }).click();
    await expect(page).toHaveURL(/\/playbooks\/drafts$/);
    expect(deletes).toHaveLength(1);
  });

  /// The same action on the rail, for a draft nobody wants to open first.
  test('a draft is deleted from the rail behind the same confirm', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/playbooks/drafts');

    const deletes: string[] = [];
    await page.route('**/api/playbook-drafts/studio', (route: Route) => {
      if (route.request().method() !== 'DELETE') return route.fallback();
      deletes.push(route.request().url());
      return route.fulfill({ status: 204, body: '' });
    });

    await page.getByRole('button', { name: 'DELETE', exact: true }).click();
    const confirm = page.getByTestId('draft-delete-confirm');
    await expect(confirm).toContainText('Delete draft studio?');
    await confirm.getByRole('button', { name: 'DELETE', exact: true }).click();
    await expect(confirm).toHaveCount(0);
    expect(deletes).toHaveLength(1);
  });
});

test.describe('editor preferences', () => {
  /// The document is server-side, so it is not the browser that remembers it: a second context
  /// with no storage of its own reads the same settings back.
  test('a preference written in the Display menu comes back in a fresh browser', async ({
    browser,
  }) => {
    let stored: Record<string, unknown> = {};
    const serve = async (page: Page) => {
      await stubApi(page);
      await page.route('**/api/prefs/editor', (route: Route) => {
        if (route.request().method() === 'PUT') {
          const sent: unknown = JSON.parse(route.request().postData() ?? '{}');
          if (typeof sent === 'object' && sent !== null && 'prefs' in sent) {
            const prefs = (sent as { prefs: unknown }).prefs;
            if (typeof prefs === 'object' && prefs !== null) {
              stored = prefs as Record<string, unknown>;
            }
          }
          return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
        }
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify({ prefs: stored }),
        });
      });
    };

    const first = await browser.newContext();
    const writer = await first.newPage();
    await serve(writer);
    await ready(writer, STUDIO);
    await writer.getByRole('button', { name: /DISPLAY/ }).click();
    await writer.getByRole('button', { name: 'contrast' }).click();
    await writer.getByRole('button', { name: '15', exact: true }).click();
    await expect
      .poll(() => stored.theme, { message: 'the pick was written server-side' })
      .toBe('contrast');
    expect(stored.fontSize).toBe(15);
    await first.close();

    const second = await browser.newContext();
    const reader = await second.newPage();
    await serve(reader);
    await ready(reader, STUDIO);
    await reader.getByRole('button', { name: /DISPLAY/ }).click();
    await expect(reader.getByRole('button', { name: 'contrast' })).toHaveAttribute(
      'aria-pressed',
      'true',
    );
    await expect(reader.getByRole('button', { name: '15', exact: true })).toHaveAttribute(
      'aria-pressed',
      'true',
    );
    await second.close();
  });
  /// The picker ranks the catalog for the newest save's [agent]: the exact fit is the default,
  /// the excluded image names the predicate that excluded it, and picking pins the digest into
  /// the manifest buffer rather than a moving tag.
  test('picking a sandbox image pins its digest into the manifest and names exclusions', async ({
    page,
  }) => {
    await stubApi(page);
    await ready(page, STUDIO);
    await treeItem(page, 'crucible.toml').click();
    await expect(await editor(page)).toContainText('[workflow]');

    const excluded = page.getByTestId('studio-excluded-images');
    await expect(excluded).toContainText('sandbox-rust-cc');
    await expect(excluded).toContainText('lacks toolchain.go');
    await expect(excluded).toContainText('custom-sandbox');

    await page.locator('#studio-sandbox-image').click();
    await page.getByRole('option', { name: /sandbox-go-cc/ }).click();
    await expect(await editor(page)).toContainText(
      'sandbox_image = "ghcr.io/acme/sandbox-go-cc@sha256:1111',
    );
    await expect(page.getByText('save first')).toBeVisible();
  });
});
