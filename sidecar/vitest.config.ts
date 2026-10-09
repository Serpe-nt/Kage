import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    include: ['packages/*/src/**/*.test.ts'],
    environment: 'node',
    // The integration tests start real Chrome, one browser per context, on
    // its own virtual display: files run one after another so that a CI
    // runner with two cores is not asked for a dozen browsers at once.
    fileParallelism: false,
    coverage: {
      provider: 'v8',
      // Every source file counts, including files no test imports: an
      // untested module shows up as 0 % instead of being left out.
      include: ['packages/*/src/**/*.ts'],
      exclude: [
        'packages/*/src/**/*.test.ts',
        // Test support: the fixture site launcher, the test proxy, the fake engine.
        'packages/*/src/testing/**',
        // Process entry points only wire tested modules together.
        'packages/server/src/main.ts',
        'packages/server/src/healthcheck.ts',
        'packages/contract/src/bin.ts',
      ],
      reporter: ['text', 'lcov', 'json-summary'],
      reportsDirectory: 'coverage',
      thresholds: {
        // Spec §21 asks for at least 80 % line coverage on engine-core. The
        // packages M4 filled in are held to the same line: the contract,
        // the server, and the Chromium engine, whose integration tests run
        // against real Chrome.
        'packages/engine-core/src/**': { lines: 80 },
        'packages/contract/src/**': { lines: 80 },
        'packages/server/src/**': { lines: 80 },
        'packages/engine-chromium/src/**': { lines: 80 },
      },
    },
  },
});
