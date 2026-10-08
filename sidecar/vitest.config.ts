import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    include: ['packages/*/src/**/*.test.ts'],
    environment: 'node',
    coverage: {
      provider: 'v8',
      // Every source file counts, including files no test imports: an
      // untested module shows up as 0 % instead of being left out.
      include: ['packages/*/src/**/*.ts'],
      // Process entry points only wire tested modules together.
      exclude: [
        'packages/*/src/**/*.test.ts',
        'packages/server/src/main.ts',
        'packages/server/src/healthcheck.ts',
      ],
      reporter: ['text', 'lcov', 'json-summary'],
      reportsDirectory: 'coverage',
      thresholds: {
        // Spec §21: at least 80 % line coverage on engine-core; the other
        // packages are reported without a gate.
        'packages/engine-core/src/**': { lines: 80 },
      },
    },
  },
});
