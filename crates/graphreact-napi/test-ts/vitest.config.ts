import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    // Default vitest config covers contract tests; benches live in
    // `bench/` and run via the dedicated `bench` script (vitest config
    // file `vitest.bench.config.ts`).
    include: ['tests/**/*.test.ts'],
    pool: 'forks',
    poolOptions: { forks: { singleFork: true } },
    testTimeout: 10000,
  },
});
