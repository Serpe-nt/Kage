import { createRequire } from 'node:module';

import { describe, expect, it } from 'vitest';

import { createEngine } from './index.js';

const require = createRequire(import.meta.url);

function installedVersion(packageName: string): string {
  const manifest = require(`${packageName}/package.json`) as { version: string };
  return manifest.version;
}

describe('engine-firefox', () => {
  it('creates the firefox engine', () => {
    expect(createEngine().name).toBe('firefox');
  });
});

describe('engine-firefox pins (fingerprint-critical, see docs/versions.md)', () => {
  it('resolves camoufox-js at its exact pin', () => {
    expect(installedVersion('camoufox-js')).toBe('0.12.1');
  });

  it('resolves a playwright-core below 1.63.0, as camoufox-js requires', () => {
    const version = installedVersion('playwright-core');
    const [major, minor] = version.split('.').map(Number);

    expect(major).toBe(1);
    expect(minor).toBeLessThan(63);
    expect(version).toBe('1.62.1');
  });

  it('cannot resolve the Chromium engine dependencies: nothing is hoisted', () => {
    expect(() => require.resolve('patchright/package.json')).toThrow();
    expect(() => require.resolve('patchright-core/package.json')).toThrow();
  });
});
