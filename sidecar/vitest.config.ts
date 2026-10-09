import { fileURLToPath } from 'node:url';

import { defineConfig } from 'vitest/config';

/** A file of a workspace package, by its path below `packages/`. */
const source = (relative: string) =>
  fileURLToPath(new URL(`./packages/${relative}`, import.meta.url));

export default defineConfig({
  resolve: {
    // Tests run the packages from their sources, whichever package imports
    // them: the engines' integration tests then count toward the coverage
    // of the code they share (engine-core's session, for one), and a test
    // never runs against a build that is older than the source next to it.
    alias: [
      {
        find: /^@kage\/(engine-core|engine-chromium)\/testing$/,
        replacement: source('$1/src/testing/index.ts'),
      },
      {
        find: /^@kage\/(contract|engine-core|engine-chromium|engine-firefox)$/,
        replacement: source('$1/src/index.ts'),
      },
    ],
  },
  test: {
    include: ['packages/*/src/**/*.test.ts'],
    environment: 'node',
    // The integration tests start real browsers, one per context, each on
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
        'packages/*/src/**/*.d.ts',
        // Test support: the fixture site launcher, the test proxy, the
        // parity suite, the capture tool, the fake engine.
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
        // other packages are held to the same line: the contract, the
        // server, and both engines, whose integration tests run against
        // their real browsers.
        'packages/engine-core/src/**': { lines: 80 },
        'packages/contract/src/**': { lines: 80 },
        'packages/server/src/**': { lines: 80 },
        'packages/engine-chromium/src/**': { lines: 80 },
        'packages/engine-firefox/src/**': { lines: 80 },
      },
    },
  },
});
