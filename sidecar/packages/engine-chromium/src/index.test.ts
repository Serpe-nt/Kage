import { createRequire } from 'node:module';

import { describe, expect, it } from 'vitest';

import { createEngine } from './index.js';

const require = createRequire(import.meta.url);

function installedVersion(packageName: string): string {
  const manifest = require(`${packageName}/package.json`) as { version: string };
  return manifest.version;
}

describe('engine-chromium', () => {
  it('creates the chromium engine', () => {
    expect(createEngine().name).toBe('chromium');
  });
});

describe('engine-chromium pins (fingerprint-critical, see docs/versions.md)', () => {
  it('resolves patchright at its exact pin', () => {
    expect(installedVersion('patchright')).toBe('1.63.0');
  });

  it('cannot resolve the Firefox engine dependencies: nothing is hoisted', () => {
    expect(() => require.resolve('playwright-core/package.json')).toThrow();
    expect(() => require.resolve('camoufox-js/package.json')).toThrow();
  });
});
