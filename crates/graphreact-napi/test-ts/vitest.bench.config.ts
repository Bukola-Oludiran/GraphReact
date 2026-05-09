import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    include: ['bench/**/*.bench.ts'],
    pool: 'forks',
    poolOptions: { forks: { singleFork: true } },
    // Benches print timing — give them a comfortable budget but no need
    // for the multi-second timing dispersion criterion would.
    testTimeout: 60_000,
  },
});
