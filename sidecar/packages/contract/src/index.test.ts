import { describe, expect, it } from 'vitest';

import { EngineNameSchema, HEALTH_PATH, HealthResponseSchema } from './index.js';

const healthy = {
  status: 'ok',
  engine: 'chromium',
  browser_version: null,
  contexts_in_use: 0,
  uptime_seconds: 1.5,
};

describe('contract', () => {
  it('serves health under the versioned prefix', () => {
    expect(HEALTH_PATH).toBe('/v1/health');
  });

  it('knows exactly the two engines', () => {
    expect(EngineNameSchema.options).toEqual(['chromium', 'firefox']);
  });

  it('accepts a health response with and without a browser version', () => {
    expect(HealthResponseSchema.parse(healthy)).toEqual(healthy);
    expect(
      HealthResponseSchema.parse({ ...healthy, engine: 'firefox', browser_version: '152.0.4' }),
    ).toMatchObject({ engine: 'firefox', browser_version: '152.0.4' });
  });

  it.each([
    ['an unknown engine', { ...healthy, engine: 'webkit' }],
    ['a status other than ok', { ...healthy, status: 'degraded' }],
    ['a negative context count', { ...healthy, contexts_in_use: -1 }],
    ['a fractional context count', { ...healthy, contexts_in_use: 0.5 }],
    ['an empty browser version', { ...healthy, browser_version: '' }],
    ['a negative uptime', { ...healthy, uptime_seconds: -1 }],
    ['an undeclared field', { ...healthy, extra: true }],
  ])('rejects %s', (_label, body) => {
    expect(HealthResponseSchema.safeParse(body).success).toBe(false);
  });
});
