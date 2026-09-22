import { defineConfig, devices } from '@playwright/test';

export default defineConfig({
  testDir: './e2e',
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? [['github'], ['html', { open: 'never' }]] : [['list']],
  expect: {
    toHaveScreenshot: { maxDiffPixelRatio: 0.001, animations: 'disabled' },
  },
  use: {
    baseURL: 'http://localhost:5273',
    trace: 'on-first-retry',
    colorScheme: 'light',
    // Screenshots capture the settled frame: the chart post-process short-circuits its animation
    // under reduced motion, so a baseline is not racing a sweep.
    reducedMotion: 'reduce',
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'], viewport: { width: 1440, height: 900 } } }],
  webServer: {
    command: 'bunx vite --port 5273 --strictPort',
    url: 'http://localhost:5273',
    reuseExistingServer: !process.env.CI,
    timeout: 60_000,
  },
});
