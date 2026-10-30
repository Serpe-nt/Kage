import { describe, expect, it } from 'vitest';
import { z } from 'zod';

import * as contract from './index.js';
import {
  BehaviorModeSchema,
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
  MAX_PAGE_LINKS,
  METRICS_PATH,
  NavigateRequestSchema,
  OpenContextRequestSchema,
  PageResultSchema,
  ScrollEndSchema,
  SelftestRequestSchema,
  contextPath,
  routePath,
  schemaId,
  stillConflicts,
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
    // 1.2.0 added the pace of a flow (M9). 1.3.0 added what a crawl needs (M10): the patience
    // of a page, the click on a link of the open page, and the links of a document. 1.4.0
    // added the move of a stored fingerprint to a new browser major (the Camoufox 156
    // milestone), an endpoint of its own. 1.5.0 adds what the camouflage levels need
    // (M14): the still visit of a navigation (`behavior.mode`) and a paced flow whose hand
    // rests while it dwells (`behavior.pace.wander`). 1.6.0 adds how many times a
    // self-test page was loaded (`attempts` of a page report, F95). All of it is optional
    // in a request and additional in an answer.
    expect(CONTRACT_SEMVER).toBe('1.6.0');
    expect(HEALTH_PATH).toBe('/v1/health');
    expect(METRICS_PATH).toBe('/v1/metrics');
    for (const endpoint of Object.values<Endpoint>(ENDPOINTS)) {
      expect(endpoint.path.startsWith('/v1/')).toBe(true);
    }
  });

  it('knows exactly the two engines', () => {
    expect(EngineNameSchema.options).toEqual(['chromium', 'firefox']);
  });

  it('has the eleven endpoints of spec §10.2', () => {
    const table = Object.values<Endpoint>(ENDPOINTS).map(
      (endpoint) => `${endpoint.method} ${endpoint.path}`,
    );
    expect(table).toEqual([
      'GET /v1/health',
      'GET /v1/metrics',
      'POST /v1/personas/generate',
      'POST /v1/personas/upgrade',
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
    ).toBe(11);
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

  it('takes the pace of a flow, with or without its two times, and nothing it does not name', () => {
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
      // `wander` joined the pace in 1.5.0; a field of a navigation's behavior did not.
      { dwell_ms: 4000, scroll_depth: 0.5 },
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

  it('lets the hand of a paced flow rest through its dwells (1.5.0), and takes a pace without it as before', () => {
    const base = first('FlowRequest');
    const paced = (pace: unknown) =>
      FlowRequestSchema.safeParse({ ...base, behavior: { seed: '5f'.repeat(32), pace } }).success;
    // Absent is true, as every pace was before 1.5.0.
    expect(paced({ dwell_ms: 4000 })).toBe(true);
    expect(paced({ dwell_ms: 4000, wander: true })).toBe(true);
    expect(paced({ dwell_ms: 4000, min_dwell_ms: 1500, wander: false })).toBe(true);
    expect(paced({ wander: false })).toBe(true);
    for (const refused of [null, 'false', 0, 1, {}]) {
      expect(paced({ wander: refused }), JSON.stringify(refused)).toBe(false);
    }
    expect(FlowPaceSchema.parse({ wander: false })).toEqual({ wander: false });
    // The examples of 1.4.0 name none, and are taken unchanged; the one of 1.5.0 rests.
    const [paced14, empty14, resting] = EXAMPLES['FlowRequest'] ?? [];
    for (const example of [paced14, empty14]) {
      expect(FlowRequestSchema.parse(example)).toEqual(example);
      expect(JSON.stringify(example)).not.toContain('"wander"');
    }
    expect(FlowRequestSchema.parse(resting)).toMatchObject({
      behavior: { pace: { wander: false } },
    });
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

  it('takes a patience, a click on the open page and a wish for the links, each inside its bounds', () => {
    const base = first('NavigateRequest');
    const withBehavior = (behavior: unknown) =>
      NavigateRequestSchema.safeParse({ ...base, behavior }).success;
    const withEntry = (entry: Record<string, unknown>) =>
      withBehavior({
        entry: { strategy: 'link', from_url: 'https://www.example.org/', ...entry },
      });
    expect(
      [1000, 38_000, 600_000].every((patience) => withBehavior({ patience_ms: patience })),
    ).toBe(true);
    for (const refused of [999, 600_001, 38_000.5, null, '38000']) {
      expect(withBehavior({ patience_ms: refused }), JSON.stringify(refused)).toBe(false);
    }
    expect(withEntry({ from_open_page: true })).toBe(true);
    expect(withEntry({ from_open_page: true, max_backs: 0, fallback: 'none' })).toBe(true);
    expect(withEntry({ from_open_page: false, max_backs: 10 })).toBe(true);
    for (const refused of [
      { from_open_page: 'yes' },
      { from_open_page: null },
      { max_backs: -1 },
      { max_backs: 11 },
      { max_backs: 1.5 },
      { backs: 1 },
    ]) {
      expect(withEntry(refused), JSON.stringify(refused)).toBe(false);
    }
    const capture = base['capture'] as Record<string, unknown>;
    const withLinks = (links: unknown) =>
      NavigateRequestSchema.safeParse({ ...base, capture: { ...capture, links } }).success;
    expect([true, false].every(withLinks)).toBe(true);
    expect([null, 'all', 1].some(withLinks)).toBe(false);
    // A request of 1.2.0 names none of it and is taken as it was.
    expect(capture).not.toHaveProperty('links');
    expect(NavigateRequestSchema.safeParse(base).success).toBe(true);
  });

  it('takes a still visit (1.5.0) with a typed entry and a fixed dwell, and nothing that needs a hand', () => {
    const base = first('NavigateRequest');
    const seed = '5f'.repeat(32);
    const issues = (behavior: unknown) => {
      const parsed = NavigateRequestSchema.safeParse({ ...base, dwell_ms: 1500, behavior });
      return parsed.success ? [] : parsed.error.issues.map((issue) => issue.path.join('.'));
    };
    const takes = (behavior: unknown) => issues(behavior).length === 0;
    // Absent, `human` and `still` are all taken; absent is `human`, as it was in 1.4.0.
    expect(BehaviorModeSchema.options).toEqual(['human', 'still']);
    expect(takes({ seed })).toBe(true);
    expect(takes({ seed, mode: 'human' })).toBe(true);
    expect(takes({ seed, mode: 'still' })).toBe(true);
    expect(takes({ mode: 'still' })).toBe(true);
    // A typed entry, by either name, and the fields a still visit leaves as they are.
    for (const strategy of ['direct', 'none'] as const) {
      expect(takes({ mode: 'still', entry: { strategy } }), strategy).toBe(true);
    }
    expect(
      takes({ mode: 'still', entry: { strategy: 'direct', from_open_page: false }, wander: false }),
    ).toBe(true);
    expect(takes({ mode: 'still', dwell: 'fixed' })).toBe(true);
    // Everything that asks for input, or for a time only input fills, is refused,
    // and the refusal names the field.
    const link = { strategy: 'link', from_url: 'https://www.example.org/' };
    for (const [behavior, path] of [
      [{ scroll_depth: 0.5 }, 'behavior.scroll_depth'],
      [{ scroll_depth: 0 }, 'behavior.scroll_depth'],
      [{ patience_ms: 30_000 }, 'behavior.patience_ms'],
      [{ wander: true }, 'behavior.wander'],
      [{ dwell: 'human' }, 'behavior.dwell'],
      [{ entry: link }, 'behavior.entry.strategy'],
      [{ entry: { ...link, strategy: 'search_engine' } }, 'behavior.entry.strategy'],
      [{ entry: { strategy: 'direct', from_open_page: true } }, 'behavior.entry.from_open_page'],
    ] as const) {
      expect(issues({ seed, mode: 'still', ...behavior }), path).toEqual([path]);
      // The same fields in a human visit are what they were.
      expect(takes({ seed, mode: 'human', ...behavior }), path).toBe(true);
      expect(takes({ seed, ...behavior }), path).toBe(true);
    }
    expect(
      issues({
        mode: 'still',
        scroll_depth: 0.4,
        wander: true,
        entry: { ...link, from_open_page: true },
      }),
    ).toEqual([
      'behavior.scroll_depth',
      'behavior.wander',
      'behavior.entry.strategy',
      'behavior.entry.from_open_page',
    ]);
    for (const refused of ['Still', 'idle', '', null, true]) {
      expect(takes({ mode: refused }), JSON.stringify(refused)).toBe(false);
    }
    // The same rule, for an engine that checks again.
    expect(stillConflicts({ mode: 'still', wander: true })).toEqual([
      { path: ['wander'], message: 'a still visit does not move the mouse' },
    ]);
    expect(stillConflicts({ mode: 'human', wander: true, scroll_depth: 1 })).toEqual([]);
    expect(stillConflicts({ scroll_depth: 1 })).toEqual([]);
    // A request of 1.4.0 names no mode and is taken as it was, its example included.
    for (const example of EXAMPLES['NavigateRequest'] ?? []) {
      expect(NavigateRequestSchema.parse(example)).toEqual(example);
    }
    expect(base['behavior']).toBeUndefined();
    const [, human] = EXAMPLES['NavigateRequest'] ?? [];
    expect((human as { behavior: object }).behavior).not.toHaveProperty('mode');
    // A flow has no mode.
    expect(
      FlowRequestSchema.safeParse({ ...first('FlowRequest'), behavior: { mode: 'still' } }).success,
    ).toBe(false);
  });

  it('always says how a page was scrolled, how it was reached and which links it holds, or null', () => {
    const page = first('PageResult');
    const behavior = page['behavior'] as Record<string, unknown>;
    const entry = behavior['entry'] as Record<string, unknown>;
    const parses = (change: Record<string, unknown>) =>
      PageResultSchema.safeParse({ ...page, ...change }).success;
    const behaves = (change: Record<string, unknown>) =>
      parses({ behavior: { ...behavior, ...change } });
    expect(parses({})).toBe(true);
    expect(ScrollEndSchema.options).toEqual(['depth', 'patience', 'budget', 'page']);
    expect(MAX_PAGE_LINKS).toBe(5000);
    for (const end of ScrollEndSchema.options) {
      expect(behaves({ scroll_ended_by: end }), end).toBe(true);
    }
    expect(behaves({ patience_ms: null, scroll_reached: null, scroll_ended_by: null })).toBe(true);
    expect(behaves({ scroll_reached: 0 })).toBe(true);
    expect(behaves({ scroll_reached: 1 })).toBe(true);
    for (const refused of [
      { scroll_reached: 1.01 },
      { scroll_reached: -0.1 },
      { scroll_ended_by: 'timeout' },
      { patience_ms: -1 },
      { entry: { ...entry, backs: -1 } },
      { entry: { ...entry, backs: 0.5 } },
    ]) {
      expect(behaves(refused), JSON.stringify(refused)).toBe(false);
    }
    expect(behaves({ entry: { ...entry, backs: null } })).toBe(true);
    const [link] = page['links'] as Record<string, unknown>[];
    expect(parses({ links: null })).toBe(true);
    expect(parses({ links: [] })).toBe(true);
    expect(parses({ links: Array.from({ length: MAX_PAGE_LINKS }, () => link) })).toBe(true);
    expect(parses({ links: Array.from({ length: MAX_PAGE_LINKS + 1 }, () => link) })).toBe(false);
    expect(parses({ links: [{ ...link, visible: 'yes' }] })).toBe(false);
    expect(parses({ links: [{ ...link, hidden: true }] })).toBe(false);
    // The fields are always there: a client reads null, never nothing.
    const without = (whole: Record<string, unknown>, name: string) =>
      Object.fromEntries(Object.entries(whole).filter(([field]) => field !== name));
    for (const name of ['patience_ms', 'scroll_reached', 'scroll_ended_by']) {
      expect(parses({ behavior: without(behavior, name) }), name).toBe(false);
    }
    for (const name of ['links', 'links_truncated']) {
      expect(PageResultSchema.safeParse(without(page, name)).success, name).toBe(false);
    }
    expect(behaves({ entry: without(entry, 'backs') })).toBe(false);
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
