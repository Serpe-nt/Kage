/**
 * One or more example values per schema, keyed by schema id.
 *
 * They are written into the OpenAPI document (`examples` of each component
 * schema) and serve three purposes: documentation, a test that every
 * example is valid against its zod schema, and the Rust contract test, which
 * reads every example into the generated type and writes it back unchanged.
 */

const SEED = '5f1c0e9a7b3d2468ace013579bdf02468ace13579bdf0246813579bdf02468ac';
const PERSONA_ID = '0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e';
const CONTEXT_ID = '0199c1f3-0a1b-7d2c-8e3f-4a5b6c7d8e9f';
const SHA = '9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08';
const USER_AGENT =
  'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36';

const screen = { width: 1920, height: 1080, dpr: 1, color_depth: 24 };
const proxy = {
  server: 'http://203.0.113.10:6540',
  username: 'user-1',
  password: 'not-a-real-password',
};
const geolocation = { latitude: 52.52, longitude: 13.405, accuracy_m: 35 };

const capture = {
  dom: true,
  raw: true,
  xhr: { content_types: ['application/json'], max_bytes: 2_000_000 },
  console: true,
  screenshot: { full_page: false, format: 'webp', quality: 80 },
  pdf: null,
};

const artifact = {
  path: `sidecar/${CONTEXT_ID}/0199c1f4-screenshot.webp`,
  bytes: 48_211,
  sha256: SHA,
  content_type: 'image/webp',
};

const pageResult = {
  url: 'https://example.org/listing?page=2',
  final_url: 'https://www.example.org/listing?page=2',
  status: 200,
  http_version: 'h2',
  headers: [
    { name: 'content-type', value: 'text/html; charset=utf-8' },
    { name: 'set-cookie', value: 'sid=abc; Path=/; Secure; HttpOnly' },
  ],
  redirects: [
    {
      url: 'https://example.org/listing?page=2',
      status: 301,
      location: 'https://www.example.org/listing?page=2',
    },
  ],
  title: 'Listing, page 2',
  dom_html: '<!DOCTYPE html><html><head><title>Listing, page 2</title></head><body></body></html>',
  dom_truncated: false,
  raw_body: {
    bytes: 71,
    truncated: false,
    encoding: 'utf8',
    data: '<!doctype html><title>Listing, page 2</title><script src="/app.js"></script>',
  },
  xhr: [
    {
      url: 'https://www.example.org/api/items?page=2',
      method: 'GET',
      kind: 'fetch',
      status: 200,
      content_type: 'application/json',
      body: { bytes: 17, truncated: false, encoding: 'utf8', data: '{"items":[1,2,3]}' },
    },
  ],
  xhr_dropped: 0,
  console: [
    {
      level: 'error',
      source: 'network',
      text: 'Failed to load resource: the server responded with a status of 404 ()',
      url: 'https://www.example.org/missing.png',
    },
  ],
  console_capture: 'browser_log',
  timing: { ttfb_ms: 410, dom_content_loaded_ms: 980, load_ms: 2210, total_ms: 3400 },
  challenge: {
    vendors: [],
    script_hosts: ['www.example.org'],
    iframe_hosts: [],
    interstitial: false,
    marker: null,
    resolved: null,
    waited_ms: 0,
  },
  screenshot: artifact,
  pdf: null,
  side_effects: [],
  error: null,
  behavior: {
    seed: SEED,
    profile_id: '3451761c17e3a615',
    entry: {
      strategy: 'link',
      outcome: 'arrived',
      from_url: 'https://www.example.org/',
      referer: 'https://www.example.org/',
      backs: 1,
    },
    dwell_ms: 6840,
    moves: 3,
    positions: 141,
    overshoots: 1,
    clicks: 1,
    notches: 9,
    keys: 0,
    typos: 0,
    wanders: 1,
    patience_ms: 41_200,
    scroll_reached: 0.62,
    scroll_ended_by: 'depth',
  },
  links: [
    {
      url: 'https://www.example.org/listing?page=3',
      text: 'Next page',
      rel: ['next'],
      visible: true,
      furniture: false,
    },
    {
      url: 'https://www.example.org/do-not-follow',
      text: '',
      rel: ['nofollow'],
      visible: false,
      furniture: false,
    },
  ],
  links_truncated: false,
};

const failedPageResult = {
  ...pageResult,
  final_url: 'https://example.org/listing?page=2',
  status: null,
  http_version: null,
  headers: [],
  redirects: [],
  title: null,
  dom_html: null,
  raw_body: null,
  xhr: [],
  console: [],
  console_capture: 'not_requested',
  timing: { ttfb_ms: null, dom_content_loaded_ms: null, load_ms: null, total_ms: 60_012 },
  screenshot: null,
  behavior: null,
  links: null,
  error: {
    code: 'timeout',
    message: 'the navigation did not finish within 60000 ms',
    net_error: null,
  },
};

const cookie = {
  domain: 'example.org',
  host_only: false,
  path: '/',
  name: 'sid',
  value: 'abc',
  secure: true,
  http_only: true,
  same_site: 'lax',
  expires_at: '2027-01-01T00:00:00Z',
};

const summary = {
  engine: 'chromium',
  claimed_os: 'linux',
  browser_version: '155.0.8059.39',
  browser_major: 155,
  user_agent: USER_AGENT,
  platform: 'Linux x86_64',
  screen,
  hardware_concurrency: 8,
  languages: ['de-DE', 'de', 'en-US', 'en'],
  generator: 'fingerprint-generator@2.1.88',
  seed: SEED,
  inject: 'none',
};

const fingerprint = {
  schema: 1,
  generator: 'fingerprint-generator@2.1.88',
  seed: SEED,
  claimed_os: 'linux',
  browser_version: '155.0.8059.39',
  native: { screen, hardware_concurrency: 8 },
  inject: 'none',
};

/** Example values by schema id. */
export const EXAMPLES: Readonly<Record<string, readonly unknown[]>> = {
  ErrorResponse: [{ code: 'context_limit', message: 'this container already runs 3 contexts' }],
  HealthResponse: [
    {
      status: 'ok',
      engine: 'chromium',
      browser_version: '155.0.8059.39',
      contexts_in_use: 1,
      max_contexts: 3,
      uptime_seconds: 86.4,
      contract_version: '1.0.0',
    },
    {
      status: 'ok',
      engine: 'firefox',
      browser_version: null,
      contexts_in_use: 0,
      max_contexts: 3,
      uptime_seconds: 0,
      contract_version: '1.0.0',
    },
  ],
  GeneratePersonaRequest: [
    { seed: SEED, claimed_os: 'linux', languages: ['de-DE', 'de', 'en-US', 'en'] },
    {
      seed: SEED,
      claimed_os: 'linux',
      languages: ['fr-FR', 'fr', 'en'],
      browser_major: 155,
      screen: { min_width: 1366, max_width: 2560 },
    },
  ],
  GeneratePersonaResponse: [{ fingerprint, summary }],
  UpgradePersonaRequest: [{ fingerprint }],
  UpgradePersonaResponse: [
    { fingerprint, summary, upgraded: true },
    { fingerprint, summary, upgraded: false },
  ],
  OpenContextRequest: [
    {
      persona_id: PERSONA_ID,
      fingerprint,
      proxy,
      locale: 'de-DE',
      languages: ['de-DE', 'de', 'en-US', 'en'],
      timezone: 'Europe/Berlin',
      geolocation,
      screen,
      profile_dir: `personas/${PERSONA_ID}`,
    },
    {
      persona_id: PERSONA_ID,
      fingerprint,
      proxy,
      locale: 'en-GB',
      languages: ['en-GB', 'en'],
      timezone: 'Europe/London',
      geolocation: null,
      screen,
      profile_dir: `personas/${PERSONA_ID}`,
    },
  ],
  OpenContextResponse: [
    {
      context_id: CONTEXT_ID,
      persona_id: PERSONA_ID,
      reused: false,
      browser_version: '155.0.8059.39',
      applied: {
        claimed_os: 'linux',
        user_agent: USER_AGENT,
        screen,
        timezone: 'Europe/Berlin',
        languages: ['de-DE', 'de', 'en-US', 'en'],
        hardware_concurrency: 4,
        gl: 'llvmpipe',
        inject: 'none',
        script_sha256: null,
      },
      opened_at: '2026-10-09T15:04:05.123Z',
    },
  ],
  NavigateRequest: [
    {
      url: 'https://example.org/listing?page=2',
      wait: { until: 'network_idle' },
      capture: { dom: true, raw: false, xhr: null, console: false, screenshot: null, pdf: null },
    },
    {
      url: 'https://example.org/listing?page=2',
      wait: { until: 'load', selector: '#results', extra_ms: 250 },
      capture: { ...capture, pdf: { format: 'a4', landscape: false, print_background: true } },
      timeout_ms: 60_000,
      challenge_wait_ms: 15_000,
      dwell_ms: 0,
      max_body_bytes: 10_000_000,
      artifact_prefix: 'requests/0199c1f4-aaaa',
      behavior: {
        seed: SEED,
        entry: { strategy: 'link', from_url: 'https://www.example.org/', fallback: 'stop' },
        scroll_depth: 0.8,
        dwell: 'human',
        wander: true,
      },
    },
    {
      // A still visit (1.5.0): the persona's browser loads the page, rests
      // for the dwell Kage drew, and sends no input at all.
      url: 'https://example.org/listing?page=2',
      wait: { until: 'load' },
      capture: { dom: true, raw: false, xhr: null, console: false, screenshot: null, pdf: null },
      timeout_ms: 45_000,
      dwell_ms: 2400,
      behavior: { seed: SEED, mode: 'still', entry: { strategy: 'direct' } },
    },
    {
      // A crawl's next page (1.7.0): the crawl names its tab, so that the
      // link is clicked on the page the crawl left open in it.
      url: 'https://example.org/listing?page=3',
      wait: { until: 'load' },
      capture: { dom: true, raw: false, xhr: null, console: false, screenshot: null, pdf: null },
      timeout_ms: 60_000,
      behavior: {
        seed: SEED,
        entry: {
          strategy: 'link',
          from_url: 'https://example.org/listing?page=2',
          fallback: 'none',
          from_open_page: true,
          max_backs: 3,
        },
      },
      tab: '0199c1f4-bbbb-7c3e-9d41-2f8a5b6c7d8e',
    },
  ],
  PageResult: [pageResult, failedPageResult],
  FlowRequest: [
    {
      actions: [
        { action: 'goto', url: 'https://example.org/' },
        { action: 'type', selector: 'input[name=q]', text: 'used cars', human: true },
        { action: 'press', key: 'Enter' },
        { action: 'wait_for', selector: '#results', timeout_ms: 15_000 },
        { action: 'scroll', depth: 0.8 },
        { action: 'click', selector: 'a.next' },
        { action: 'select', selector: 'select#sort', value: 'price' },
        { action: 'hover', selector: '#menu' },
        { action: 'back' },
        { action: 'extract', capture },
        { action: 'screenshot', options: { full_page: false, format: 'png' } },
        { action: 'pdf', options: {} },
        { action: 'assert', text_contains: 'results' },
      ],
      capture,
      timeout_ms: 180_000,
      behavior: { seed: SEED, typos: true, pace: { dwell_ms: 5000, min_dwell_ms: 1500 } },
    },
    {
      actions: [
        { action: 'goto', url: 'https://example.org/listing?page=2' },
        { action: 'click', selector: 'a.next' },
        { action: 'back', human: false },
        { action: 'wait_for', url: '**/listing?page=2' },
        { action: 'extract', capture },
      ],
      capture,
      // An empty `pace`: the persona's own dwell, no floor.
      behavior: { pace: {} },
    },
    {
      actions: [
        { action: 'goto', url: 'https://example.org/' },
        { action: 'click', selector: 'a.next' },
        { action: 'extract', capture },
      ],
      capture,
      // A paced flow whose hand rests while it dwells (1.5.0): no input but the click.
      behavior: { seed: SEED, pace: { dwell_ms: 4000, wander: false } },
    },
  ],
  FlowEvent: [
    {
      type: 'step',
      index: 0,
      action: 'goto',
      ok: true,
      duration_ms: 1840,
      paced_ms: null,
      error: null,
      capture: null,
    },
    {
      type: 'step',
      index: 3,
      action: 'wait_for',
      ok: false,
      duration_ms: 15_000,
      paced_ms: null,
      error: 'no element matched #results within 15000 ms',
      capture: null,
    },
    { type: 'final', ok: true, result: pageResult, behavior: pageResult.behavior },
    { type: 'final', ok: false, result: failedPageResult, behavior: null },
    {
      // A step of a paced flow: a click that left the page, and the dwell on the page it led to.
      type: 'step',
      index: 5,
      action: 'click',
      ok: true,
      duration_ms: 8470,
      paced_ms: 6930,
      error: null,
      capture: null,
    },
    {
      // A step of a paced flow that only reads is never delayed.
      type: 'step',
      index: 6,
      action: 'wait_for',
      ok: true,
      duration_ms: 212,
      paced_ms: 0,
      error: null,
      capture: null,
    },
  ],
  ExportStateRequest: [
    { origin: 'https://example.org' },
    { origin: 'https://example.org:8443', include_storage: true },
  ],
  ExportStateResponse: [
    {
      origin: 'https://example.org',
      cookies: [
        cookie,
        { ...cookie, name: 'theme', host_only: true, same_site: null, expires_at: null },
      ],
      local_storage: [{ name: 'seen', value: '1' }],
      session_storage: null,
    },
  ],
  ImportStateRequest: [{ origin: 'https://example.org', cookies: [cookie] }],
  ImportStateResponse: [
    { imported: 1, rejected: [{ name: 'other', domain: 'example.com', reason: 'foreign_domain' }] },
  ],
  SelftestRequest: [
    {
      expected: {
        claimed_os: 'linux',
        browser_major: 155,
        timezone: 'Europe/Berlin',
        languages: ['de-DE', 'de', 'en-US', 'en'],
      },
    },
    {
      pages: [
        { id: 'probe', url: 'https://echo.example.org/probe/', kind: 'probe' },
        { id: 'echo', url: 'https://echo.example.org/', kind: 'echo', weight: 2 },
        { id: 'sannysoft', url: 'https://bot.sannysoft.com/', kind: 'sannysoft', settle_ms: 4000 },
        {
          id: 'custom',
          url: 'https://detector.example.org/',
          kind: 'generic',
          rules: [
            { check: 'not_a_bot', pattern: 'you are (a )?bot', expect: 'no_match', hard: true },
          ],
        },
      ],
      expected: {
        claimed_os: 'linux',
        browser_major: 155,
        timezone: 'Europe/Berlin',
        languages: ['de-DE', 'de', 'en-US', 'en'],
      },
      echo: {
        ja4: 't13d1516h2_8daaf6152771_d8a2da3f94cd',
        akamai: '1:65536;2:0;4:6291456;6:262144|15663105|0|m,a,s,p',
        headers: ['sec-ch-ua', 'sec-ch-ua-mobile', 'sec-ch-ua-platform', 'user-agent', 'accept'],
      },
      min_score: 90,
      timeout_ms: 45_000,
      artifact_prefix: 'selftests/0199c1f5',
    },
  ],
  SelftestResponse: [
    {
      passed: true,
      complete: false,
      score: 100,
      min_score: 90,
      findings: [
        {
          page: 'probe',
          check: 'navigator.webdriver',
          status: 'pass',
          hard: true,
          weight: 1,
          observed: 'false',
          expected: 'false',
        },
        {
          page: 'sannysoft',
          check: 'page',
          status: 'unknown',
          hard: false,
          weight: 1,
          observed: null,
          expected: null,
        },
      ],
      pages: [
        {
          id: 'probe',
          url: 'https://echo.example.org/probe/',
          kind: 'probe',
          status: 'ok',
          http_status: 200,
          duration_ms: 2130,
          attempts: 1,
          text: { ...artifact, path: 'selftests/0199c1f5/probe.txt', content_type: 'text/plain' },
          screenshot: {
            ...artifact,
            path: 'selftests/0199c1f5/probe.png',
            content_type: 'image/png',
          },
        },
        {
          id: 'sannysoft',
          url: 'https://bot.sannysoft.com/',
          kind: 'sannysoft',
          status: 'unreachable',
          http_status: null,
          // Two loads of 45 s and the pause between them (1.6.0).
          duration_ms: 95_000,
          attempts: 2,
          text: null,
          screenshot: null,
        },
      ],
      started_at: '2026-10-09T15:04:05Z',
      finished_at: '2026-10-09T15:05:42.500Z',
    },
  ],
};
