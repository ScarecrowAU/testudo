import { defineConfig } from 'vitest/config';
import { playwright } from '@vitest/browser-playwright';

// Same browser-mode setup Storybook's addon-vitest generates.
export default defineConfig({
  test: {
    browser: {
      enabled: true,
      headless: true,
      provider: playwright(),
      instances: [{ browser: 'chromium' }],
    },
  },
});
