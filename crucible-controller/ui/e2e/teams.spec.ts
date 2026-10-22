import { expect, test, type Page, type Route } from '@playwright/test';
import { ROUTES, stubApi } from './api';

async function ready(page: Page, path: string): Promise<void> {
  const crashes: string[] = [];
  page.on('pageerror', (e) => crashes.push(String(e)));
  await page.goto(path);
  await page.waitForLoadState('networkidle');
  expect(crashes, `uncaught exceptions on ${path}:\n${crashes.join('\n')}`).toEqual([]);
  await expect(page.locator('main')).toBeVisible();
}

async function switchTo(page: Page, value: string): Promise<void> {
  await page.getByTestId('owner-switcher').click();
  await page.getByTestId('owner-switcher-menu').locator(`[data-value="${value}"]`).click();
}

test.describe('acting as a team', () => {
  test('choosing a team acts as it: every request names it and the rail follows its lanes', async ({ page }) => {
    const named: (string | undefined)[] = [];
    page.on('request', (req) => {
      if (new URL(req.url()).pathname === '/api/whoami') named.push(req.headers()['x-crucible-act-as']);
    });
    await stubApi(page);
    await ready(page, '/');
    const rail = page.getByTestId('category-rail');
    await expect(rail.getByRole('link', { name: 'Dashboard' })).toBeVisible();
    await expect(page.getByTestId('owner-switcher')).not.toHaveAttribute('data-acting-as');

    await switchTo(page, 'team:llm-d');
    await expect(page.getByTestId('owner-switcher')).toHaveAttribute('data-acting-as', 'team:llm-d');
    await expect(page.getByTestId('owner-switcher')).toContainText('Acting as');
    await expect(rail.getByRole('link', { name: 'Dashboard' })).toHaveCount(0);
    expect(named.at(-1)).toBe('team:llm-d');

    await switchTo(page, 'user:wren');
    await expect(rail.getByRole('link', { name: 'Dashboard' })).toBeVisible();
    await expect(page.getByTestId('owner-switcher')).not.toHaveAttribute('data-acting-as');
    expect(named.at(-1)).toBeUndefined();
  });

  test('a stored team the controller refuses drops back to every owner and says why', async ({ page }) => {
    await page.addInitScript(() => {
      window.localStorage.setItem('crucible.owner.context', 'team:ghost');
    });
    await stubApi(page);
    await page.goto('/');
    await expect(page.getByTestId('owner-switcher')).toContainText('All');
    await expect(page.getByTestId('owner-switcher')).not.toHaveAttribute('data-acting-as');
    await expect(page.getByTestId('category-rail').getByRole('link', { name: 'Dashboard' })).toBeVisible();
    await expect(page.getByTestId('act-as-refused')).toHaveText('you are not a member of team:ghost');

    await switchTo(page, 'user:wren');
    await expect(page.getByTestId('act-as-refused')).toHaveCount(0);
  });
});

test.describe('owner context', () => {
  test('the switcher lists the user and each team, and the lists follow it', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/playbooks');

    const rows = page.locator('main tbody tr');
    await expect(rows).toHaveCount(2);
    await expect(page.getByTestId('owner-switcher')).toContainText('All');

    await page.getByTestId('owner-switcher').click();
    const menu = page.getByTestId('owner-switcher-menu');
    await expect(menu.locator('[data-value]')).toHaveText([
      'All',
      'user:wren',
      'team:llm-dmaintainer',
      'team:platform-administratorsowner',
    ]);
    await menu.locator('[data-value="team:llm-d"]').click();

    await expect(page.getByTestId('owner-switcher')).toContainText('llm-d');
    await expect(rows).toHaveCount(1);
    await expect(rows.first()).toContainText('survey');
    await expect(page.locator('main')).toContainText('Showing 1');

    await switchTo(page, 'user:wren');
    await expect(rows).toHaveCount(1);
    await expect(rows.first()).toContainText('triage');

    // The context is per device: it survives navigation.
    await page.goto('/playbooks/drafts');
    await page.waitForLoadState('networkidle');
    await expect(page.getByTestId('owner-switcher')).toContainText('wren');
    await expect(page.locator('main tbody tr')).toHaveCount(1);
    await switchTo(page, 'team:llm-d');
    await expect(page.locator('main')).toContainText('No drafts');
    await expect(page.locator('main')).toContainText('Showing 0');
  });

  test('a creation form starts on the context and offers only principals that may own', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/playbooks/drafts');
    await switchTo(page, 'team:llm-d');

    const owner = page.locator('#draft-owner');
    await expect(owner).toHaveValue('team:llm-d');
    await expect(owner.locator('option')).toHaveText([
      'user:wren',
      'team:llm-d (maintainer)',
      'team:platform-administrators (owner)',
      'group:/groups/platform',
    ]);
    await owner.selectOption('user:wren');
    await expect(owner).toHaveValue('user:wren');

    await switchTo(page, 'all');
    await expect(owner).toHaveValue('user:wren');
  });

  test('a refused creation shows the decision verbatim', async ({ page }) => {
    await stubApi(page);
    await page.route('**/api/playbook-drafts', (route: Route) =>
      route.request().method() === 'POST'
        ? route.fulfill({
            status: 403,
            contentType: 'application/json',
            body: JSON.stringify({
              error: 'user:wren may not playbook_draft:create (no-rule)',
              rule: 'no-rule',
            }),
          })
        : route.fallback(),
    );
    await ready(page, '/playbooks/drafts');

    await page.locator('#draft-id').fill('mine');
    await page.locator('#draft-description').fill('a pack');
    await page.getByRole('button', { name: 'Create draft' }).click();
    await expect(page.locator('main')).toContainText('user:wren may not playbook_draft:create (no-rule)');
  });
});

test.describe('teams', () => {
  test('settings shows every membership with how it is held', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/settings');

    const memberships = page.getByTestId('memberships');
    await expect(memberships).toContainText('team:llm-d');
    await expect(memberships).toContainText('maintainer');
    await expect(memberships).toContainText('group /groups/platform · maintainer');
    await expect(memberships).toContainText('rule configured-admins · owner');
  });

  test('a team page lists members by role with how each is held', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/teams');
    await expect(page.locator('main tbody tr')).toHaveCount(2);
    await page.locator('main tbody tr').first().getByRole('link', { name: 'llm-d' }).click();

    await expect(page).toHaveURL(/\/teams\/llm-d$/);
    await page.locator('main').getByRole('link', { name: '4', exact: true }).click();
    await expect(page).toHaveURL(/\/teams\/llm-d\/members$/);
    await expect(page.locator('main')).toContainText('LLM-D');
    const cells = page.locator('main tbody tr');
    await expect(cells).toHaveCount(4);
    await expect(cells.nth(0)).toContainText('wren');
    await expect(cells.nth(0)).toContainText('direct');
    await expect(cells.nth(0)).toContainText('owner');
    await expect(cells.nth(1)).toContainText('/groups/platform');
    await expect(cells.nth(1)).toContainText('group');
    await expect(cells.nth(2)).toContainText('core');
    await expect(cells.nth(2)).toContainText('nested team');
    await expect(cells.nth(3)).toContainText('email-domain:example.com');
    await expect(cells.nth(3)).toContainText('rule');
  });
});

test.describe('team workspace', () => {
  test('a team page is its home, and the rail collapses to the team', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/teams/llm-d');

    const rail = page.getByTestId('category-rail');
    await expect(rail.getByRole('heading', { name: 'llm-d' })).toBeVisible();
    for (const [label, href] of [
      ['Home', '/teams/llm-d'],
      ['Playbooks', '/teams/llm-d/playbooks'],
      ['Drafts', '/teams/llm-d/playbooks/drafts'],
      ['Runs', '/teams/llm-d/playbook-runs'],
      ['Schedules', '/teams/llm-d/schedules'],
      ['Webhooks', '/teams/llm-d/webhooks'],
      ['Secrets', '/teams/llm-d/secrets'],
      ['Providers', '/teams/llm-d/providers'],
      ['Members', '/teams/llm-d/members'],
    ]) {
      await expect(rail.getByRole('link', { name: label, exact: true })).toHaveAttribute('href', href);
    }
    await expect(rail.getByRole('link', { name: 'All teams', exact: true })).toHaveAttribute('href', '/teams');
    for (const global of ['Activity', 'Admin', 'Approvals']) {
      await expect(rail.getByRole('link', { name: global, exact: true })).toHaveCount(0);
    }
    await expect(rail.getByRole('link', { name: 'platform-administrators', exact: true })).toBeVisible();
    await expect(rail.getByRole('link', { name: 'Everything', exact: true })).toBeVisible();

    const main = page.locator('main');
    await expect(main).toContainText('survey');
    await expect(main).not.toContainText('triage');
    await expect(main.getByRole('link', { name: /All runs/ })).toHaveAttribute('href', '/teams/llm-d/playbook-runs');

    await expect(page.getByTestId('owner-switcher')).toContainText('All');
    await expect(page.getByTestId('owner-switcher')).not.toHaveAttribute('data-acting-as');

    await rail.getByRole('link', { name: 'Everything', exact: true }).click();
    await expect(rail.getByRole('link', { name: 'Activity', exact: true })).toBeVisible();
    await expect(main).toContainText('triage');
  });

  test('a team link opens straight onto the team\'s view of each list', async ({ page }) => {
    await stubApi(page);
    const secrets = ROUTES['/api/secrets'];
    if (!Array.isArray(secrets) || typeof secrets[0] !== 'object' || secrets[0] === null) throw new Error('secret stub');
    const teamSecret = { ...secrets[0], id: 'sec-team', name: 'hf_token', owner: 'team:llm-d' };
    await page.route('**/api/secrets', (route: Route) =>
      route.fulfill({ contentType: 'application/json', body: JSON.stringify([...secrets, teamSecret]) }),
    );

    await ready(page, '/teams/llm-d/playbooks');
    await expect(page.locator('main tbody tr')).toHaveCount(1);
    await expect(page.locator('main tbody tr').first()).toContainText('survey');
    await expect(page.getByTestId('category-rail').getByRole('link', { name: 'Playbooks', exact: true })).toHaveAttribute(
      'aria-current',
      'page',
    );

    await ready(page, '/teams/llm-d/playbook-runs');
    await expect(page.locator('main')).toContainText('survey');
    await expect(page.locator('main')).not.toContainText('kylesayrs');

    await ready(page, '/teams/llm-d/secrets');
    await expect(page.locator('main').getByRole('cell', { name: 'hf_token', exact: true })).toBeVisible();
    await expect(page.locator('main').getByRole('cell', { name: 'pr_token', exact: true })).toHaveCount(0);

    await ready(page, '/teams/llm-d/members');
    await expect(page.locator('main')).toContainText('Members');
    await expect(page.getByTestId('category-rail').getByRole('link', { name: 'Members', exact: true })).toHaveAttribute(
      'aria-current',
      'page',
    );
  });

  test('acting as a team keeps its rail everywhere, and the switcher is the way out', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/teams/llm-d');
    await page.getByTestId('act-as-team').click();
    await expect(page.getByTestId('act-as-team')).toHaveText('Acting as');
    await expect(page.getByTestId('owner-switcher')).toHaveAttribute('data-acting-as', 'team:llm-d');

    await ready(page, '/');
    const rail = page.getByTestId('category-rail');
    await expect(rail.getByRole('heading', { name: 'llm-d' })).toBeVisible();
    await expect(rail.getByRole('link', { name: 'Everything', exact: true })).toHaveCount(0);
    await expect(page.locator('main')).not.toContainText('triage');

    await switchTo(page, 'all');
    await expect(rail.getByRole('link', { name: 'Activity', exact: true })).toBeVisible();
  });
});

test.describe('many teams', () => {
  /// Group claims put the caller in ten more teams that own nothing.
  async function manyTeams(page: Page): Promise<void> {
    await stubApi(page);
    const whoami = ROUTES['/api/whoami'];
    const teams = ROUTES['/api/teams'];
    if (typeof whoami !== 'object' || whoami === null || Array.isArray(whoami) || !Array.isArray(whoami.teams)) {
      throw new Error('whoami stub');
    }
    if (!Array.isArray(teams) || typeof teams[1] !== 'object' || teams[1] === null || Array.isArray(teams[1])) {
      throw new Error('teams stub');
    }
    const claims = Array.from({ length: 10 }, (_, i) => `claim-${String(i).padStart(2, '0')}`);
    const memberships = [
      ...whoami.teams,
      ...claims.map((team) => ({ team, role: 'member', via: [{ kind: 'group', group: `/groups/${team}`, role: 'member' }] })),
    ];
    const rows = [
      ...teams,
      ...claims.map((slug) => ({ ...teams[1], slug, display_name: slug.toUpperCase(), my_role: 'member' })),
      { ...teams[1], slug: 'not-mine', display_name: 'Not mine', my_role: null },
    ];
    await page.route('**/api/whoami', (route: Route) =>
      route.request().headers()['x-crucible-act-as'] === undefined
        ? route.fulfill({ contentType: 'application/json', body: JSON.stringify({ ...whoami, teams: memberships }) })
        : route.fallback(),
    );
    await page.route('**/api/teams', (route: Route) =>
      route.fulfill({ contentType: 'application/json', body: JSON.stringify(rows) }),
    );
    let prefs: unknown = {};
    await page.route('**/api/prefs/pickers', (route: Route) => {
      if (route.request().method() === 'PUT') {
        const sent: unknown = JSON.parse(route.request().postData() ?? '{}');
        if (typeof sent === 'object' && sent !== null && 'prefs' in sent) prefs = sent.prefs;
      }
      return route.fulfill({ contentType: 'application/json', body: JSON.stringify({ prefs }) });
    });
  }

  test('the rail and the switcher name only starred and active teams', async ({ page }) => {
    await manyTeams(page);
    await ready(page, '/');

    const rail = page.getByTestId('category-rail');
    await expect(rail.getByRole('link', { name: 'llm-d', exact: true })).toBeVisible();
    await expect(rail.getByRole('link', { name: 'claim-03', exact: true })).toHaveCount(0);
    await expect(rail.getByRole('link', { name: /^All teams/ })).toContainText('11');

    await page.getByTestId('owner-switcher').click();
    const menu = page.getByTestId('owner-switcher-menu');
    await expect(menu.locator('[data-value]')).toHaveText(['All', 'user:wren', 'team:llm-dmaintainer']);
    await expect(page.getByTestId('owner-switcher-more')).toContainText('11 more');
    await page.getByTestId('owner-switcher-more').click();
    await expect(page).toHaveURL(/\/teams$/);

    await expect(page.locator('main tbody tr')).toHaveCount(12);
    await page.getByRole('searchbox', { name: 'Filter teams' }).fill('CLAIM-03');
    await expect(page.locator('main tbody tr')).toHaveCount(1);
    await page.getByRole('button', { name: 'Star claim-03' }).click();
    await expect(page.getByRole('button', { name: 'Unstar claim-03' })).toHaveAttribute('aria-pressed', 'true');

    await expect(rail.getByRole('link', { name: '★ claim-03', exact: true })).toBeVisible();
    await expect(rail.getByRole('link', { name: /^All teams/ })).toContainText('10');

    await page.reload();
    await page.waitForLoadState('networkidle');
    await expect(rail.getByRole('link', { name: '★ claim-03', exact: true })).toBeVisible();
    await page.getByTestId('owner-switcher').click();
    await expect(page.getByTestId('owner-switcher-menu').locator('[data-value]')).toHaveText([
      'All',
      'user:wren',
      'team:claim-03member',
      'team:llm-dmaintainer',
    ]);
    await page.keyboard.press('Escape');

    // A team star is the owner picker's star: the same principal reads starred there.
    await page.getByRole('searchbox', { name: 'Filter teams' }).fill('llm-d');
    await page.getByRole('button', { name: 'Star llm-d' }).click();
    await expect(page.getByRole('button', { name: 'Unstar llm-d' })).toBeVisible();
    await ready(page, '/secrets');
    await expect(page.getByRole('button', { name: 'Unstar team:llm-d (maintainer)' })).toHaveAttribute('aria-pressed', 'true');
  });

  test('the teams page shows memberships by default and every readable team on demand', async ({ page }) => {
    await manyTeams(page);
    await ready(page, '/teams');
    const rows = page.locator('main tbody tr');
    await expect(rows).toHaveCount(12);
    await expect(page.locator('main')).not.toContainText('not-mine');
    await page.locator('main [data-ui="toolbar-group"]').getByRole('button', { name: 'All', exact: true }).click();
    await expect(rows).toHaveCount(13);
    await expect(rows.first()).toContainText('llm-d');
    await expect(rows.last()).toContainText('not-mine');
  });
});

test.describe('team view polish', () => {
  test('a team list names the team, and its secrets page drops the personal split', async ({ page }) => {
    await stubApi(page);
    await ready(page, '/teams/llm-d/secrets');
    const main = page.locator('main');
    await expect(main.getByText('llm-d', { exact: true }).first()).toBeVisible();
    await expect(main).not.toContainText('Personal');
    await expect(main).toContainText('No team secrets');

    await ready(page, '/teams/llm-d');
    await expect(main).not.toContainText('waiting for approval');
    await ready(page, '/');
    await expect(main).toContainText('waiting for approval');
  });
});
