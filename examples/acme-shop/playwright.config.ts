import { defineConfig, devices } from '@playwright/test';

// End-to-end tests against self-contained shop pages (no server, runs offline).
export default defineConfig({
  testDir: './e2e',
  fullyParallel: true,
  workers: 3,
  reporter: 'list',
  use: { viewport: { width: 1024, height: 700 } },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'], viewport: { width: 1024, height: 700 } } }],
});
