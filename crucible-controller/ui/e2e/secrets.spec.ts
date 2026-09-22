import { expect, test, type Page, type Route } from '@playwright/test';
import { stubApi } from './api';

const SECRETS = '/secrets';
const OWN_ID = '0199-secret-a';

async function ready(page: Page, path: string): Promise<void> {
  const crashes: string[] = [];
  page.on('pageerror', (e) => crashes.push(String(e)));
  await page.goto(path);
  await page.waitForLoadState('networkidle');
  expect(crashes, `uncaught exceptions on ${path}:\n${crashes.join('\n')}`).toEqual([]);
  await expect(page.locator('main')).toBeVisible();
}

/// Record every body the page sends, so a test can assert what left the browser as well as what
/// came back.
async function recordWrites(page: Page, sent: string[]): Promise<void> {
  await page.route('**/api/secrets**', (route: Route) => {
    const request = route.request();
    if (request.method() === 'GET') return route.fallback();
    sent.push(`${request.method()} ${new URL(request.url()).pathname} ${request.postData() ?? ''}`);
    if (request.method() === 'DELETE') return route.fulfill({ status: 204, body: '' });
    return route.fulfill({
      status: 201,
      contentType: 'application/json',
      body: JSON.stringify({
        id: 'new-secret',
        name: 'pr_token',
        owner: 'user:wren',
        kind: 'opaque',
        visibility: 'broker_only',
        consumer: 'run',
        mode: 'managed',
        vault_path: 'user:wren/pr_token',
        current_version: 1,
        created_by: 'wren',
        created_at: '2026-08-25T09:00:00Z',
        updated_at: '2026-08-25T09:00:00Z',
      }),
    });
  });
}

test.describe('the secrets page', () => {
  test("separates the caller's own secrets from their teams'", async ({ page }) => {
    await stubApi(page);
    await ready(page, SECRETS);

    const own = page.getByText('pr_token', { exact: true }).first();
    await expect(own).toBeVisible();
    await expect(page.getByRole('cell', { name: 'group:/groups/platform' })).toBeVisible();
    // A location, never a value: the reference row shows the pointer it holds.
    await expect(
      page.getByRole('cell', { name: 'vault://kv/platform/quay#authfile' }),
    ).toBeVisible();
  });

  test('registers by value and renders no value field afterwards', async ({ page }) => {
    const sent: string[] = [];
    await stubApi(page);
    await recordWrites(page, sent);
    await ready(page, SECRETS);

    await page.locator('#secret-name').fill('pr_token');
    await page.locator('#secret-value').fill('the-sentinel-value');
    await page.getByRole('button', { name: 'REGISTER' }).click();

    await expect(page.getByText('pr_token is registered', { exact: false })).toBeVisible();
    expect(sent.join('\n')).toContain('"value":"the-sentinel-value"');
    // The form is cleared and no response field carries it back.
    await expect(page.locator('#secret-value')).toHaveValue('');
    expect(await page.locator('body').innerText()).not.toContain('the-sentinel-value');
  });

  test('never renders a secret-bearing field as plain text', async ({ page }) => {
    await stubApi(page);
    await ready(page, SECRETS);

    await expect(page.locator('#secret-value')).toHaveAttribute('type', 'password');

    await page.locator('#secret-mode').selectOption('reference');
    await expect(page.locator('#secret-vault-token')).toHaveAttribute('type', 'password');

    // A file's bytes need the height of a textarea, which has no password type, so it is masked
    // and opened only by the toggle.
    await page.locator('#secret-mode').selectOption('value');
    await page.locator('#secret-kind').selectOption('file');
    const area = page.locator('textarea#secret-value');
    const security = () => area.evaluate((el) => getComputedStyle(el).webkitTextSecurity);
    await expect.poll(security).toBe('disc');
    await page.getByRole('button', { name: 'SHOW' }).click();
    await expect.poll(security).toBe('none');
    await page.getByRole('button', { name: 'HIDE' }).click();
    await expect.poll(security).toBe('disc');
  });

  test('warns before an agent_visible value can be saved', async ({ page }) => {
    await stubApi(page);
    await ready(page, SECRETS);

    await page.locator('#secret-name').fill('pr_token');
    await page.locator('#secret-value').fill('a-token');
    await expect(page.getByRole('button', { name: 'REGISTER' })).toBeEnabled();

    await page.locator('#secret-visibility').selectOption('agent_visible');
    await expect(page.getByText('can end up in a pull request', { exact: false })).toBeVisible();
    await expect(page.getByRole('button', { name: 'REGISTER' })).toBeDisabled();

    await page.getByRole('checkbox').click();
    await expect(page.getByRole('button', { name: 'REGISTER' })).toBeEnabled();
  });

  test('offers agent_visible only for an opaque secret', async ({ page }) => {
    await stubApi(page);
    await ready(page, SECRETS);

    await page.locator('#secret-kind').selectOption('registry_authfile');
    await expect(page.locator('#secret-visibility option')).toHaveCount(1);
    await expect(page.locator('#secret-visibility')).toHaveValue('broker_only');
  });

  test('binds a secret to a scope and unbinds it again', async ({ page }) => {
    const sent: string[] = [];
    await stubApi(page);
    await page.route(`**/api/secrets/${OWN_ID}`, (route: Route) =>
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          id: OWN_ID,
          name: 'pr_token',
          owner: 'user:wren',
          kind: 'opaque',
          visibility: 'broker_only',
          consumer: 'run',
          mode: 'managed',
          vault_path: 'user:wren/pr_token',
          current_version: 2,
          created_by: 'wren',
          created_at: '2026-08-23T09:00:00Z',
          updated_at: '2026-08-24T09:00:00Z',
          bindings: [
            {
              id: 'binding-1',
              secret_id: OWN_ID,
              scope_kind: 'repo',
              scope_id: 'neuralmagic/crucible',
              projection_kind: 'env',
              projection: 'AUTORESEARCH_PR_TOKEN',
              declared_name: 'pr_token',
              pack_rev: null,
              schema_digest: null,
              created_by: 'wren',
              created_at: '2026-08-24T09:00:00Z',
            },
          ],
        }),
      }),
    );
    await page.route(`**/api/secrets/${OWN_ID}/audit`, (route: Route) =>
      route.fulfill({ status: 200, contentType: 'application/json', body: '[]' }),
    );
    await recordWrites(page, sent);
    await ready(page, SECRETS);

    await page.getByRole('button', { name: 'pr_token' }).first().click();
    await expect(page.getByText('repo neuralmagic/crucible — pr_token as env AUTORESEARCH_PR_TOKEN')).toBeVisible();
    // A bound secret cannot be deleted, and the refusal names what is holding it.
    await expect(page.getByText('unbind it first', { exact: false })).toBeVisible();
    await expect(page.getByRole('button', { name: 'DELETE' })).toBeDisabled();

    await page.locator('#bind-scope-id').fill('vllm-project/vllm');
    await page.locator('#bind-projection').fill('VLLM_PR_TOKEN');
    await page.getByRole('button', { name: 'BIND', exact: true }).click();
    await expect
      .poll(() => sent.join('\n'))
      .toContain('"scope_id":"vllm-project/vllm"');

    await page.getByRole('button', { name: 'UNBIND' }).click();
    await expect
      .poll(() => sent.join('\n'))
      .toContain(`DELETE /api/secrets/${OWN_ID}/bindings/binding-1`);
  });
});
