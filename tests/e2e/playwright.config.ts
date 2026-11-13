import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: '.',
  testMatch: 'website.spec.ts',
  workers: 1,
  fullyParallel: false,
  timeout: 60_000,
  expect: { timeout: 15_000 },
  use: {
    browserName: 'chromium',
    headless: true,
    trace: 'retain-on-failure',
  },
  reporter: 'list',
});
