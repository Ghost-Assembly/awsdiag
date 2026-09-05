import { defineConfig, devices } from '@playwright/test';

// Chromium and Firefox only: WebKit is not supported on this Fedora base.
export default defineConfig({
  testDir: './tests/browser',
  fullyParallel: true,
  reporter: [['list']],
  use: { trace: 'off' },
  projects: [
    { name: 'chromium', use: { ...devices['Desktop Chrome'] } },
    { name: 'firefox', use: { ...devices['Desktop Firefox'] } },
  ],
});
