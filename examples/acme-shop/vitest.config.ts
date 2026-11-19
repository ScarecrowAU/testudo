import { defineConfig } from 'vitest/config';

// Unit tests for the shop's logic, in Node (no browser): Testudo shows one
// output tile per test file.
export default defineConfig({
  test: {
    include: ['src/**/*.test.ts'],
    environment: 'node',
  },
});
