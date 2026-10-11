import { describe, expect, it } from 'vitest';
import { z } from 'zod';

import * as contract from './index.js';
import {
  CONTRACT_SEMVER,
  CONTRACT_VERSION,
  COMMON_ERRORS,
  CookieSchema,
  ENDPOINTS,
  ERROR_STATUS,
  EXAMPLES,
  EngineNameSchema,
  ErrorCodeSchema,
  FlowActionSchema,
  FlowEventSchema,
  FlowPaceSchema,
  FlowRequestSchema,
  GeneratePersonaRequestSchema,
  HEALTH_PATH,
  HealthResponseSchema,
  MAX_PACE_DWELL_MS,
  MAX_PACE_MIN_DWELL_MS,
  METRICS_PATH,
  NavigateRequestSchema,
  OpenContextRequestSchema,
  SelftestRequestSchema,
  contextPath,
  routePath,
  schemaId,
  type Endpoint,
} from './index.js';

/** Every exported zod schema that has an id, by that id. */
function namedSchemas(): Map<string, z.ZodType> {
  const byId = new Map<string, z.ZodType>();
  for (const value of Object.values(contract)) {
    if (value instanceof z.ZodType) {
      const id = z.globalRegistry.get(value)?.id;
      if (id !== undefined) {
        byId.set(id, value);
      }
    }
  }
  return byId;
}

const first = (id: string): Record<string, unknown> => {
  const [example] = EXAMPLES[id] ?? [];
  return structuredClone(example) as Record<string, unknown>;
};

describe('contract basics', () => {
  it('serves everything under the versioned prefix', () => {
    expect(CONTRACT_VERSION).toBe('v1');
    expect(CONTRACT_SEMVER.startsWith('1.')).toBe(true);
    // 1.2.0 added the pace of a flow (M9): optional in the request, additional in the answer.
    expect(CONTRACT_SEMVER).toBe('1.2.0');
    expect(HEALTH_PATH).toBe('/v1/health');
    expect(METRICS_PATH).toBe('/v1/metrics');
    for (const endpoint of Object.values<Endpoint>(ENDPOINTS)) {
      expect(endpoint.path.startsWith('/v1/')).toBe(true);
    }
  });

  it('knows exactly the two engines', () => {
    expect(EngineNameSchema.options).toEqual(['chromium', 'firefox']);
  });

  it('has the ten endpoints of spec §10.2', () => {
    const table = Object.values<Endpoint>(ENDPOINTS).map(
      (endpoint) => `${endpoint.method} ${endpoint.path}`,
    );
    expect(table).toEqual([
      'GET /v1/health',
      'GET /v1/metrics',
      'POST /v1/personas/generate',
      'POST /v1/contexts',
      'DELETE /v1/contexts/{id}',
      'POST /v1/contexts/{id}/navigate',
      'POST /v1/contexts/{id}/flow',
      'POST /v1/contexts/{id}/state/export',
      'POST /v1/contexts/{id}/state/import',
      'POST /v1/contexts/{id}/selftest',
    ]);
    expect(
      new Set(Object.values<Endpoint>(ENDPOINTS).map((endpoint) => endpoint.operation)).size,
    ).toBe(10);
  });

  it('gives every error code one status and every endpoint known codes', () => {
    expect(Object.keys(ERROR_STATUS).sort()).toEqual([...ErrorCodeSchema.options].sort());
    for (const status of Object.values(ERROR_STATUS)) {
      expect(status).toBeGreaterThanOrEqual(400);
      expect(status).toBeLessThan(600);
    }
    for (const endpoint of Object.values<Endpoint>(ENDPOINTS)) {
      for (const code of [...endpoint.errors, ...COMMON_ERRORS]) {
        expect(ErrorCodeSchema.options).toContain(code);
      }
    }
  });

  it('writes paths for fastify and for a client', () => {
    expect(routePath(ENDPOINTS.navigate)).toBe('/v1/contexts/:id/navigate');
    expect(routePath(ENDPOINTS.health)).toBe('/v1/health');
    expect(contextPath(ENDPOINTS.closeContext, 'a b')).toBe('/v1/contexts/a%20b');
  });

  it('names every schema an endpoint uses', () => {
    for (const endpoint of Object.values<Endpoint>(ENDPOINTS)) {
      if (endpoint.request !== null) {
        expect(schemaId(endpoint.request)).toMatch(/Request$/);
      }
      if (endpoint.response.schema !== null) {
        expect(schemaId(endpoint.response.schema)).toMatch(/^[A-Z]/);
      }
    }
    expect(() => schemaId(z.string())).toThrow(/has no id/);
  });
});

describe('examples', () => {
  const schemas = namedSchemas();

  it('exist for every request and response of an endpoint', () => {
    for (const endpoint of Object.values<Endpoint>(ENDPOINTS)) {
      for (const schema of [endpoint.request, endpoint.response.schema]) {
        if (schema !== null) {
          expect(EXAMPLES[schemaId(schema)]?.length ?? 0, schemaId(schema)).toBeGreaterThan(0);
        }
      }
    }
  });

  it.each(Object.entries(EXAMPLES))(
    'of %s are valid and survive a parse unchanged',
    (id, examples) => {
      const schema = schemas.get(id);
      expect(schema, `${id} is not an exported schema`).toBeDefined();
      for (const example of examples) {
        // No default and no transform: what goes in comes out.
        expect(schema?.parse(example)).toEqual(example);
      }
    },
  );
});

describe('health', () => {
  const healthy = first('HealthResponse');

  it.each([
    ['an unknown engine', { engine: 'webkit' }],
    ['a status other than ok', { status: 'degraded' }],
    ['a negative context count', { contexts_in_use: -1 }],
    ['a fractional context count', { contexts_in_use: 0.5 }],
    ['an empty browser version', { browser_version: '' }],
    ['a negative uptime', { uptime_seconds: -1 }],
    ['no context cap', { max_contexts: 0 }],
    ['an undeclared field', { extra: true }],
  ])('rejects %s', (_label, change) => {
    expect(HealthResponseSchema.safeParse({ ...healthy, ...change }).success).toBe(false);
  });
});

describe('requests are strict', () => {
  it.each([
    [
      'a seed that is not 64 hexadecimal characters',
      GeneratePersonaRequestSchema,
      'GeneratePersonaRequest',
      { seed: 'abc' },
    ],
    [
      'an upper-case seed',
      GeneratePersonaRequestSchema,
      'GeneratePersonaRequest',
      { seed: 'A'.repeat(64) },
    ],
    [
      'an unknown operating system',
      GeneratePersonaRequestSchema,
      'GeneratePersonaRequest',
      { claimed_os: 'android' },
    ],
    ['no language', GeneratePersonaRequestSchema, 'GeneratePersonaRequest', { languages: [] }],
    [
      'a language that is not a tag',
      GeneratePersonaRequestSchema,
      'GeneratePersonaRequest',
      { languages: ['de_DE'] },
    ],
    [
      'a profile directory that climbs out',
      OpenContextRequestSchema,
      'OpenContextRequest',
      { profile_dir: '../etc' },
    ],
    [
      'an absolute profile directory',
      OpenContextRequestSchema,
      'OpenContextRequest',
      { profile_dir: '/var/lib/x' },
    ],
    [
      'a profile directory with a hidden part',
      OpenContextRequestSchema,
      'OpenContextRequest',
      { profile_dir: 'a/.ssh' },
    ],
    [
      'a persona id that is not a UUID',
      OpenContextRequestSchema,
      'OpenContextRequest',
      { persona_id: '42' },
    ],
    [
      'a proxy with a path',
      OpenContextRequestSchema,
      'OpenContextRequest',
      { proxy: { server: 'http://h:1/x', username: 'u', password: 'p' } },
    ],
    [
      'a SOCKS proxy',
      OpenContextRequestSchema,
      'OpenContextRequest',
      { proxy: { server: 'socks5://h:1', username: 'u', password: 'p' } },
    ],
    [
      'a proxy without credentials',
      OpenContextRequestSchema,
      'OpenContextRequest',
      { proxy: { server: 'http://h:1' } },
    ],
    [
      'a time zone with spaces',
      OpenContextRequestSchema,
      'OpenContextRequest',
      { timezone: 'Europe / Berlin' },
    ],
    [
      'a screen that is too small',
      OpenContextRequestSchema,
      'OpenContextRequest',
      { screen: { width: 10, height: 10, dpr: 1, color_depth: 24 } },
    ],
    ['a file URL', NavigateRequestSchema, 'NavigateRequest', { url: 'file:///etc/passwd' }],
    [
      'a browser-internal URL',
      NavigateRequestSchema,
      'NavigateRequest',
      { url: 'chrome://version' },
    ],
    ['a script URL', NavigateRequestSchema, 'NavigateRequest', { url: 'javascript:alert(1)' }],
    [
      'a URL with a space',
      NavigateRequestSchema,
      'NavigateRequest',
      { url: 'https://example.org/a b' },
    ],
    [
      'a timeout over the hard cap',
      NavigateRequestSchema,
      'NavigateRequest',
      { timeout_ms: 300_001 },
    ],
    [
      'an artifact prefix that climbs out',
      NavigateRequestSchema,
      'NavigateRequest',
      { artifact_prefix: 'a/../../b' },
    ],
    ['an unknown wait', NavigateRequestSchema, 'NavigateRequest', { wait: { until: 'whenever' } }],
    ['an undeclared field', NavigateRequestSchema, 'NavigateRequest', { headless: true }],
    [
      'a page id with a slash',
      SelftestRequestSchema,
      'SelftestRequest',
      { pages: [{ id: 'a/b', url: 'https://x.test/', kind: 'probe' }] },
    ],
    ['an empty page list', SelftestRequestSchema, 'SelftestRequest', { pages: [] }],
  ])('rejects %s', (_label, schema, example, change) => {
    expect(schema.safeParse(first(example)).success).toBe(true);
    expect(schema.safeParse({ ...first(example), ...change }).success).toBe(false);
  });

  it('accepts a URL at the length limit and rejects a longer one', () => {
    const base = first('NavigateRequest');
    const url = (length: number) =>
      `https://example.org/${'a'.repeat(length - 'https://example.org/'.length)}`;
    expect(NavigateRequestSchema.safeParse({ ...base, url: url(8192) }).success).toBe(true);
    expect(NavigateRequestSchema.safeParse({ ...base, url: url(8193) }).success).toBe(false);
  });

  it('chooses a flow step by its action and refuses fields of another', () => {
    expect(FlowActionSchema.parse({ action: 'back' })).toEqual({ action: 'back' });
    expect(FlowActionSchema.safeParse({ action: 'click' }).success).toBe(false);
    expect(
      FlowActionSchema.safeParse({ action: 'goto', url: 'https://x.test/', selector: 'a' }).success,
    ).toBe(false);
    expect(FlowActionSchema.safeParse({ action: 'teleport' }).success).toBe(false);
  });

  it('takes the pace of a flow, with or without its two times, and nothing else in it', () => {
    const base = first('FlowRequest');
    const paced = (pace: unknown) =>
      FlowRequestSchema.safeParse({ ...base, behavior: { seed: '5f'.repeat(32), pace } }).success;
    // Present and empty already asks for pacing: the persona's own dwell, no floor.
    expect(paced({})).toBe(true);
    expect(paced({ dwell_ms: 4000 })).toBe(true);
    expect(paced({ min_dwell_ms: 1500 })).toBe(true);
    expect(paced({ dwell_ms: 0, min_dwell_ms: 0 })).toBe(true);
    expect(paced({ dwell_ms: MAX_PACE_DWELL_MS, min_dwell_ms: MAX_PACE_MIN_DWELL_MS })).toBe(true);
    expect([MAX_PACE_DWELL_MS, MAX_PACE_MIN_DWELL_MS]).toEqual([120_000, 45_000]);
    // A flow without it, and one without any behavior, stay what they were.
    expect(FlowRequestSchema.safeParse({ ...base, behavior: { typos: true } }).success).toBe(true);
    expect(
      FlowRequestSchema.safeParse({ actions: base['actions'], capture: base['capture'] }).success,
    ).toBe(true);
    for (const refused of [
      { dwell: 4000 },
      { dwell_ms: 4000, wander: true },
      { dwell_ms: -1 },
      { dwell_ms: MAX_PACE_DWELL_MS + 1 },
      { dwell_ms: 1500.5 },
      { dwell_ms: null },
      { min_dwell_ms: -1 },
      { min_dwell_ms: MAX_PACE_MIN_DWELL_MS + 1 },
      { min_dwell_ms: '1500' },
      null,
      true,
    ]) {
      expect(paced(refused), JSON.stringify(refused)).toBe(false);
    }
    expect(FlowPaceSchema.parse({ dwell_ms: 4000 })).toEqual({ dwell_ms: 4000 });
  });

  it('says of every step of a flow how much of it was pacing, or null', () => {
    const [step] = (EXAMPLES['FlowEvent'] ?? []) as Record<string, unknown>[];
    expect(step).toMatchObject({ type: 'step', paced_ms: null });
    expect(FlowEventSchema.safeParse({ ...step, paced_ms: 0 }).success).toBe(true);
    expect(FlowEventSchema.safeParse({ ...step, paced_ms: 6120 }).success).toBe(true);
    expect(FlowEventSchema.safeParse({ ...step, paced_ms: -1 }).success).toBe(false);
    expect(FlowEventSchema.safeParse({ ...step, paced_ms: 12.5 }).success).toBe(false);
    // The field is always there: a client reads null, never nothing.
    const without = { ...step };
    delete without['paced_ms'];
    expect(FlowEventSchema.safeParse(without).success).toBe(false);
  });

  it('keeps a cookie to the fields of an http_cookies row', () => {
    const cookie = { ...(first('ImportStateRequest')['cookies'] as Record<string, unknown>[])[0] };
    expect(Object.keys(CookieSchema.parse(cookie)).sort()).toEqual([
      'domain',
      'expires_at',
      'host_only',
      'http_only',
      'name',
      'path',
      'same_site',
      'secure',
      'value',
    ]);
    expect(CookieSchema.safeParse({ ...cookie, same_site: 'Lax' }).success).toBe(false);
    expect(CookieSchema.safeParse({ ...cookie, expires_at: '2027-01-01' }).success).toBe(false);
  });
});
