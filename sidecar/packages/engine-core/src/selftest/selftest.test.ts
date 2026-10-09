import { mkdtemp, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';

import type { ExpectedIdentity, Finding, SelftestPage } from '@kage/contract';
import { describe, expect, it } from 'vitest';

import {
  ArtifactWriter,
  DEFAULT_SETTLE_MS,
  FindingList,
  PROBE_REPORT_SELECTOR,
  judgeCreepjs,
  judgeEcho,
  judgeGeneric,
  judgeProbe,
  judgeSannysoft,
  parseProbeReport,
  runSelftest,
  scoreFindings,
  verdict,
  type LoadedPage,
  type ProbeReport,
  type ProbeScope,
  type SelftestDriver,
} from '../index.js';

const LINUX_UA =
  'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36';
const WINDOWS_UA =
  'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36';

const expected: ExpectedIdentity = {
  claimed_os: 'linux',
  browser_major: 155,
  timezone: 'Europe/Berlin',
  languages: ['de-DE', 'de', 'en-US', 'en'],
};

const HIGH_ENTROPY = {
  platform: 'Linux',
  platform_version: '',
  architecture: 'x86',
  bitness: '64',
  ua_full_version: '155.0.8059.39',
};

const scope = (change: Partial<ProbeScope> = {}): ProbeScope => ({
  user_agent: LINUX_UA,
  platform: 'Linux x86_64',
  languages: ['de-DE', 'de', 'en-US', 'en'],
  hardware_concurrency: 8,
  device_memory: 16,
  timezone: 'Europe/Berlin',
  uad_platform: 'Linux',
  high_entropy: HIGH_ENTROPY,
  webgl: {
    vendor: 'Google Inc. (Mesa)',
    renderer: 'ANGLE (Mesa, llvmpipe (LLVM 20.1.2 256 bits), OpenGL 4.5)',
  },
  error: null,
  ...change,
});

const navigationHeaders: [string, string][] = [
  ['Host', 'fixture.localhost'],
  ['sec-ch-ua-platform', '"Linux"'],
  ['User-Agent', LINUX_UA],
  ['Accept', 'text/html'],
];

const report = (change: (report: ProbeReport) => void = () => undefined): ProbeReport => {
  const value: ProbeReport = {
    kage_probe: 1,
    scopes: {
      window: scope(),
      iframe_same_origin: scope(),
      iframe_srcdoc: scope(),
      iframe_blank: scope(),
      worker: scope(),
      blob_worker: scope(),
    },
    automation: {
      webdriver: false,
      console_stack_reads: 0,
      getters_native: true,
      illegal_invocation_throws: true,
      navigator_own_properties: [],
    },
    requests: {
      navigation: navigationHeaders,
      iframe: navigationHeaders,
      fetch: navigationHeaders,
      worker_script: [['User-Agent', LINUX_UA]],
      worker_fetch: [['User-Agent', LINUX_UA]],
    },
  };
  change(value);
  return value;
};

const failed = (findings: readonly Finding[]) =>
  findings.filter((finding) => finding.status === 'fail').map((finding) => finding.check);
const hardFailed = (findings: readonly Finding[]) =>
  findings
    .filter((finding) => finding.status === 'fail' && finding.hard)
    .map((finding) => finding.check);

describe('score and verdict', () => {
  const finding = (status: Finding['status'], weight = 1, hard = false): Finding => ({
    page: 'p',
    check: 'c',
    status,
    hard,
    weight,
    observed: null,
    expected: null,
  });

  it('scores fails fully, warns by half and unknowns not at all', () => {
    expect(scoreFindings([])).toBe(100);
    expect(scoreFindings([finding('unknown'), finding('unknown')])).toBe(100);
    expect(scoreFindings([finding('pass'), finding('pass')])).toBe(100);
    expect(scoreFindings([finding('pass'), finding('fail')])).toBe(50);
    expect(scoreFindings([finding('pass'), finding('warn')])).toBe(75);
    expect(scoreFindings([finding('pass'), finding('fail'), finding('unknown')])).toBe(50);
    expect(scoreFindings([finding('fail', 1), finding('pass', 2)])).toBe(66.7);
    expect(scoreFindings([finding('fail', 0), finding('pass', 1)])).toBe(100);
  });

  it('passes without a hard failure at or above the threshold', () => {
    expect(verdict([finding('pass')], 90, true)).toEqual({
      score: 100,
      passed: true,
      complete: true,
    });
    const nine = Array.from({ length: 9 }, () => finding('pass'));
    expect(verdict([...nine, finding('fail')], 90, true)).toMatchObject({
      score: 90,
      passed: true,
    });
    expect(verdict([...nine.slice(1), finding('fail')], 90, true)).toMatchObject({
      score: 88.9,
      passed: false,
    });
  });

  it('fails on one hard failure whatever the score', () => {
    const many = Array.from({ length: 99 }, () => finding('pass'));
    expect(verdict([...many, finding('fail', 1, true)], 90, true)).toMatchObject({
      score: 99,
      passed: false,
    });
    // A hard check that only warns does not fail the run.
    expect(verdict([...many, finding('warn', 1, true)], 90, true).passed).toBe(true);
  });

  it('is incomplete when a page was not read or a finding is unknown, and still passes', () => {
    expect(verdict([finding('pass'), finding('unknown')], 90, true)).toEqual({
      score: 100,
      passed: true,
      complete: false,
    });
    expect(verdict([finding('pass')], 90, false).complete).toBe(false);
  });

  it('builds findings with the page id and weight of their list', () => {
    const list = new FindingList('probe', 2);
    list.add('a', 'pass');
    list.equal('b', 'x', 'y', true);
    list.check('c', true, false, 'seen');
    list.add('d', 'warn', { weight: 0, observed: 'o', expected: 'e' });
    expect(list.findings).toEqual([
      {
        page: 'probe',
        check: 'a',
        status: 'pass',
        hard: false,
        weight: 2,
        observed: null,
        expected: null,
      },
      {
        page: 'probe',
        check: 'b',
        status: 'fail',
        hard: true,
        weight: 2,
        observed: 'x',
        expected: 'y',
      },
      {
        page: 'probe',
        check: 'c',
        status: 'pass',
        hard: false,
        weight: 2,
        observed: 'seen',
        expected: null,
      },
      {
        page: 'probe',
        check: 'd',
        status: 'warn',
        hard: false,
        weight: 0,
        observed: 'o',
        expected: 'e',
      },
    ]);
  });
});

describe('the coherence probe', () => {
  it('passes an honest Linux persona in every scope', () => {
    const findings = judgeProbe(report(), expected, 'probe', 1);
    expect(failed(findings)).toEqual([]);
    expect(findings.every((finding) => finding.status === 'pass')).toBe(true);
    expect(findings.map((finding) => finding.check)).toEqual(
      expect.arrayContaining([
        'automation.webdriver',
        'automation.console_getter',
        'automation.native_getters',
        'os.window',
        'os.worker',
        'os.blob_worker',
        'browser_major.iframe_srcdoc',
        'timezone.iframe_blank',
        'languages.iframe_same_origin',
        'coherence.user_agent',
        'coherence.platform',
        'coherence.hint_platform',
        'coherence.hardware_concurrency',
        'coherence.device_memory',
        'coherence.webgl',
        'webgl.available',
        'headers.navigation.user_agent',
        'headers.navigation.platform',
        'headers.navigation.cache',
        'headers.worker_fetch.user_agent',
      ]),
    );
    expect(verdict(findings, 90, true)).toEqual({ score: 100, passed: true, complete: true });
  });

  it('fails a Windows claim that its own workers contradict', () => {
    // What Chrome 155 on Linux shows under a user-agent override: the
    // window and the iframes say Windows, a worker keeps the Linux platform
    // and the host's processor count.
    const windows = scope({
      user_agent: WINDOWS_UA,
      platform: 'Win32',
      uad_platform: 'Windows',
      hardware_concurrency: 4,
      high_entropy: {
        platform: 'Windows',
        platform_version: '19.0.0',
        architecture: 'x86',
        bitness: '64',
        ua_full_version: '155.0.8059.39',
      },
    });
    const inWorker = { ...windows, platform: 'Linux x86_64', hardware_concurrency: 32 };
    const windowsHeaders: [string, string][] = [
      ['sec-ch-ua-platform', '"Windows"'],
      ['User-Agent', WINDOWS_UA],
    ];
    const findings = judgeProbe(
      report((value) => {
        value.scopes = {
          window: windows,
          iframe_same_origin: windows,
          iframe_srcdoc: windows,
          iframe_blank: windows,
          worker: inWorker,
          blob_worker: inWorker,
        };
        value.requests = {
          navigation: windowsHeaders,
          iframe: windowsHeaders,
          fetch: windowsHeaders,
          worker_script: [['User-Agent', WINDOWS_UA]],
          worker_fetch: [['User-Agent', WINDOWS_UA]],
        };
      }),
      { ...expected, claimed_os: 'windows' },
      'probe',
      1,
    );
    expect(hardFailed(findings).sort()).toEqual([
      'coherence.hardware_concurrency',
      'coherence.platform',
      'os.blob_worker',
      'os.worker',
    ]);
    expect(findings.find((finding) => finding.check === 'coherence.platform')?.observed).toContain(
      'worker=Linux x86_64',
    );
    expect(findings.find((finding) => finding.check === 'os.window')?.status).toBe('pass');
    expect(verdict(findings, 90, true).passed).toBe(false);
  });

  it.each<[string, (report: ProbeReport) => void, string[]]>([
    [
      'navigator.webdriver',
      (value) => (value.automation.webdriver = true),
      ['automation.webdriver'],
    ],
    [
      'a missing webdriver flag',
      (value) => (value.automation.webdriver = null),
      ['automation.webdriver'],
    ],
    [
      'a console getter read',
      (value) => (value.automation.console_stack_reads = 1),
      ['automation.console_getter'],
    ],
    [
      'a replaced getter',
      (value) => (value.automation.getters_native = false),
      ['automation.native_getters'],
    ],
    [
      'a getter that accepts any receiver',
      (value) => (value.automation.illegal_invocation_throws = false),
      ['automation.native_getters'],
    ],
    [
      'an own property on navigator',
      (value) => (value.automation.navigator_own_properties = ['userAgent']),
      ['automation.native_getters'],
    ],
    ['another time zone', (value) => (value.scopes.window.timezone = 'UTC'), ['timezone.window']],
    [
      'other languages in a worker',
      (value) => (value.scopes.worker = scope({ languages: ['en-US', 'en'] })),
      ['languages.worker'],
    ],
    [
      'another browser major',
      (value) => (value.scopes.window.user_agent = LINUX_UA.replace('155', '147')),
      // Every request header is compared with the window's user agent.
      [
        'browser_major.window',
        'coherence.user_agent',
        'headers.navigation.user_agent',
        'headers.iframe.user_agent',
        'headers.fetch.user_agent',
        'headers.worker_script.user_agent',
        'headers.worker_fetch.user_agent',
      ],
    ],
    [
      'another full version',
      (value) =>
        (value.scopes.window.high_entropy = {
          ...HIGH_ENTROPY,
          ua_full_version: '147.0.7727.137',
        }),
      ['browser_major.window'],
    ],
    [
      'a client-hint platform that differs',
      (value) => (value.scopes.window.uad_platform = 'Windows'),
      ['os.window', 'coherence.hint_platform'],
    ],
    [
      'a high-entropy platform that differs',
      (value) => (value.scopes.window.high_entropy = { ...HIGH_ENTROPY, platform: 'macOS' }),
      ['os.window'],
    ],
    [
      'a worker with another renderer',
      (value) =>
        (value.scopes.worker = scope({
          webgl: { vendor: 'Google Inc.', renderer: 'SwiftShader' },
        })),
      ['coherence.webgl'],
    ],
    [
      'a worker with another memory size',
      (value) => (value.scopes.worker = scope({ device_memory: 8 })),
      ['coherence.device_memory'],
    ],
    [
      'a header that names another platform',
      (value) =>
        (value.requests.fetch = [
          ['sec-ch-ua-platform', '"Windows"'],
          ['user-agent', LINUX_UA],
        ]),
      ['headers.fetch.platform'],
    ],
    [
      'a header with another user agent',
      (value) => (value.requests.worker_script = [['user-agent', WINDOWS_UA]]),
      ['headers.worker_script.user_agent'],
    ],
    [
      'forced cache headers',
      (value) =>
        (value.requests.navigation = [
          ...navigationHeaders,
          ['Pragma', 'no-cache'],
          ['Cache-Control', 'no-cache'],
        ]),
      ['headers.navigation.cache'],
    ],
  ])('fails hard on %s', (_label, change, checks) => {
    const findings = judgeProbe(report(change), expected, 'probe', 1);
    expect(hardFailed(findings).sort()).toEqual([...checks].sort());
  });

  it('warns about a scope that did not report and about a missing WebGL, and fails nothing', () => {
    const findings = judgeProbe(
      report((value) => {
        value.scopes.worker = null;
        value.scopes.blob_worker = scope({ error: 'SecurityError: workers are blocked' });
        value.scopes.iframe_blank = scope({ user_agent: null });
        value.scopes.window.webgl = null;
        value.scopes.iframe_same_origin = scope({ webgl: null });
        value.scopes.iframe_srcdoc = scope({ webgl: null });
      }),
      expected,
      'probe',
      1,
    );
    expect(failed(findings)).toEqual([]);
    const warned = findings.filter((finding) => finding.status === 'warn');
    expect(warned.map((finding) => finding.check)).toEqual([
      'scope.iframe_blank',
      'scope.worker',
      'scope.blob_worker',
      'webgl.available',
    ]);
    expect(warned[2]?.observed).toBe('SecurityError: workers are blocked');
    expect(verdict(findings, 90, true).passed).toBe(true);
  });

  it('judges a probe on a static host, which cannot tell request headers', () => {
    const findings = judgeProbe(
      report((value) => {
        value.requests = {
          navigation: null,
          iframe: null,
          fetch: null,
          worker_script: null,
          worker_fetch: null,
        };
      }),
      expected,
      'probe',
      1,
    );
    expect(findings.some((finding) => finding.check.startsWith('headers.'))).toBe(false);
    expect(failed(findings)).toEqual([]);
  });

  it('needs two reporting scopes to compare them', () => {
    const findings = judgeProbe(
      report((value) => {
        value.scopes = {
          window: scope({
            platform: null,
            uad_platform: null,
            high_entropy: null,
            hardware_concurrency: null,
            device_memory: null,
          }),
          iframe_same_origin: null,
          iframe_srcdoc: null,
          iframe_blank: null,
          worker: null,
          blob_worker: null,
        };
      }),
      expected,
      'probe',
      1,
    );
    expect(findings.some((finding) => finding.check.startsWith('coherence.'))).toBe(false);
    // Without a platform to read, the user agent alone cannot carry the claim.
    expect(hardFailed(findings)).toEqual(['os.window']);
  });

  it('reads a report from JSON, tolerates odd values and refuses what is none', () => {
    const json = JSON.stringify(report());
    expect(parseProbeReport(json)).toEqual(report());
    const odd = JSON.parse(json) as { scopes: { window: Record<string, unknown> }; extra?: number };
    odd.scopes.window['hardware_concurrency'] = 'eight';
    odd.scopes.window['languages'] = 'de';
    odd.extra = 1;
    const parsed = parseProbeReport(JSON.stringify(odd));
    expect(parsed?.scopes.window.hardware_concurrency).toBeNull();
    expect(parsed?.scopes.window.languages).toBeNull();
    expect(parseProbeReport('{"kage_probe":2}')).toBeNull();
    expect(parseProbeReport('not json')).toBeNull();
  });
});

describe('the fingerprint echo', () => {
  const headers = [
    ['sec-ch-ua', '"Google Chrome";v="155"'],
    ['sec-ch-ua-platform', '"Linux"'],
    ['user-agent', LINUX_UA],
    ['accept', 'text/html'],
  ];
  const answer = (change: Record<string, unknown> = {}) =>
    JSON.stringify({
      echo: 'kage-echo',
      version: 1,
      http_version: 'h2',
      tls: { ja4: 't13d1517h2_8daaf6152771_aaaaaaaaaaaa' },
      http2: { akamai: '1:65536;2:0;4:6291456;6:262144|15663105|0|m,a,s,p', headers },
      ...change,
    });
  const capture = {
    ja4: 't13d1517h2_8daaf6152771_aaaaaaaaaaaa',
    akamai: '1:65536;2:0;4:6291456;6:262144|15663105|0|m,a,s,p',
    headers: ['sec-ch-ua', 'sec-ch-ua-platform', 'user-agent', 'accept'],
  };

  it('passes a browser that equals its recorded capture', () => {
    const findings = judgeEcho(answer(), expected, capture, 'echo', 2) ?? [];
    expect(failed(findings)).toEqual([]);
    expect(findings.map((finding) => finding.check)).toEqual([
      'echo.http_version',
      'echo.ja4',
      'echo.akamai',
      'echo.header_order',
      'echo.user_agent_os',
      'echo.user_agent_major',
      'echo.hint_platform',
      'echo.cache_headers',
    ]);
    expect(findings.every((finding) => finding.weight === 2 && finding.hard)).toBe(true);
  });

  it.each<[string, Record<string, unknown>, string[]]>([
    ['another JA4', { tls: { ja4: 't13d1516h2_8daaf6152771_d8a2da3f94cd' } }, ['echo.ja4']],
    [
      'another HTTP/2 fingerprint',
      { http2: { akamai: '1:65536|0|0|m,p,a,s', headers } },
      ['echo.akamai'],
    ],
    [
      'another header order',
      { http2: { akamai: capture.akamai, headers: [...headers].reverse() } },
      ['echo.header_order'],
    ],
    [
      'forced cache headers',
      { http2: { akamai: capture.akamai, headers: [['pragma', 'no-cache'], ...headers] } },
      ['echo.header_order', 'echo.cache_headers'],
    ],
    [
      'HTTP/1.1',
      {
        http_version: 'http/1.1',
        http2: null,
        http1: { headers: headers.map(([name, value]) => [name, value]) },
      },
      ['echo.http_version', 'echo.akamai'],
    ],
    [
      'another platform hint',
      {
        http2: {
          akamai: capture.akamai,
          headers: headers.map(([name, value]) =>
            name === 'sec-ch-ua-platform' ? [name, '"Windows"'] : [name, value],
          ),
        },
      },
      ['echo.hint_platform'],
    ],
    [
      'another user agent',
      {
        http2: {
          akamai: capture.akamai,
          headers: headers.map(([name, value]) =>
            name === 'user-agent' ? [name, WINDOWS_UA.replace('155', '147')] : [name, value],
          ),
        },
      },
      ['echo.user_agent_os', 'echo.user_agent_major'],
    ],
  ])('fails hard on %s', (_label, change, checks) => {
    expect(
      hardFailed(judgeEcho(answer(change), expected, capture, 'echo', 1) ?? []).sort(),
    ).toEqual([...checks].sort());
  });

  it('only reports the transport when there is no capture to compare with', () => {
    const findings = judgeEcho(answer(), expected, null, 'echo', 1) ?? [];
    const transport = findings.filter((finding) =>
      ['echo.ja4', 'echo.akamai', 'echo.header_order'].includes(finding.check),
    );
    expect(transport.map((finding) => [finding.status, finding.weight, finding.expected])).toEqual([
      ['pass', 0, null],
      ['pass', 0, null],
      ['pass', 0, null],
    ]);
    expect(transport[0]?.observed).toBe('t13d1517h2_8daaf6152771_aaaaaaaaaaaa');
    expect(failed(findings)).toEqual([]);
  });

  it('judges an answer without a platform hint and without headers', () => {
    const findings =
      judgeEcho(
        answer({ http2: { akamai: capture.akamai, headers: [] } }),
        expected,
        null,
        'echo',
        1,
      ) ?? [];
    expect(findings.some((finding) => finding.check === 'echo.hint_platform')).toBe(false);
    expect(hardFailed(findings).sort()).toEqual(['echo.user_agent_major', 'echo.user_agent_os']);
  });

  it('refuses what is not an answer of the echo', () => {
    expect(judgeEcho('<html>', expected, capture, 'echo', 1)).toBeNull();
    expect(judgeEcho('{"echo":"other"}', expected, capture, 'echo', 1)).toBeNull();
  });
});

describe('bot.sannysoft.com', () => {
  const rows = [
    { cells: ['Test Name', 'Result'], classes: ['', ''] },
    { cells: ['User Agent (Old)', LINUX_UA], classes: ['', 'result passed'] },
    { cells: ['WebDriver (New)', 'missing (passed)'], classes: ['', 'passed'] },
    { cells: ['WebGL Renderer', 'ANGLE (Mesa, llvmpipe)'], classes: ['', 'warn'] },
    { cells: ['HEADCHR_PERMISSIONS', 'ok', '{"x":1}'], classes: ['', 'passed', 'details'] },
    { cells: ['WebDriver (New)', 'duplicate row'], classes: ['', 'failed'] },
    { cells: ['', 'no name'], classes: ['', 'passed'] },
  ];

  it('turns each result row into a finding and marks automation rows as hard', () => {
    expect(judgeSannysoft(rows, 'sannysoft', 1)).toEqual([
      {
        page: 'sannysoft',
        check: 'sannysoft.User Agent (Old)',
        status: 'pass',
        hard: false,
        weight: 1,
        observed: LINUX_UA,
        expected: null,
      },
      {
        page: 'sannysoft',
        check: 'sannysoft.WebDriver (New)',
        status: 'pass',
        hard: true,
        weight: 1,
        observed: 'missing (passed)',
        expected: null,
      },
      {
        page: 'sannysoft',
        check: 'sannysoft.WebGL Renderer',
        status: 'warn',
        hard: false,
        weight: 1,
        observed: 'ANGLE (Mesa, llvmpipe)',
        expected: null,
      },
      {
        page: 'sannysoft',
        check: 'sannysoft.HEADCHR_PERMISSIONS',
        status: 'pass',
        hard: true,
        weight: 1,
        observed: 'ok',
        expected: null,
      },
    ]);
  });

  it('fails hard on a red automation row and softly on another red row', () => {
    const findings =
      judgeSannysoft(
        [
          { cells: ['WebDriver Advanced', 'failed'], classes: ['', 'failed'] },
          { cells: ['Broken Image Dimensions', '0x0'], classes: ['', 'failed'] },
        ],
        'sannysoft',
        1,
      ) ?? [];
    expect(hardFailed(findings)).toEqual(['sannysoft.WebDriver Advanced']);
    expect(failed(findings)).toHaveLength(2);
  });

  it('is unparsed without a single result row', () => {
    expect(judgeSannysoft([], 'sannysoft', 1)).toBeNull();
    expect(judgeSannysoft([{ cells: ['a', 'b'], classes: ['', ''] }], 'sannysoft', 1)).toBeNull();
  });
});

describe('CreepJS', () => {
  it('reads the three shares in the order CreepJS prints them', () => {
    const text =
      'Headless\nchromium: true\n31% like headless: noTaskbar, hasSwiftShader\n0% headless:\n0% stealth:\nlies: none';
    expect(judgeCreepjs(text, 'creepjs', 1)).toEqual([
      {
        page: 'creepjs',
        check: 'creepjs.headless',
        status: 'pass',
        hard: true,
        weight: 1,
        observed: '0%',
        expected: '0%',
      },
      {
        page: 'creepjs',
        check: 'creepjs.stealth',
        status: 'pass',
        hard: true,
        weight: 1,
        observed: '0%',
        expected: '0%',
      },
      {
        page: 'creepjs',
        check: 'creepjs.like_headless',
        status: 'pass',
        hard: false,
        weight: 1,
        observed: '31%',
        expected: 'at most 50%',
      },
      {
        page: 'creepjs',
        check: 'creepjs.lies',
        status: 'pass',
        hard: false,
        weight: 1,
        observed: '0',
        expected: '0',
      },
    ]);
  });

  it('reads the label-first wording too and does not take "like headless" for "headless"', () => {
    const findings =
      judgeCreepjs(
        'like headless: 56%\nheadless: 33%\nstealth: 20.5%\nlies (3): navigator.userAgent',
        'creepjs',
        1,
      ) ?? [];
    expect(findings.map((finding) => [finding.check, finding.status, finding.observed])).toEqual([
      ['creepjs.headless', 'fail', '33%'],
      ['creepjs.stealth', 'fail', '20.5%'],
      ['creepjs.like_headless', 'warn', '56%'],
      ['creepjs.lies', 'fail', '3'],
    ]);
    expect(hardFailed(findings)).toEqual(['creepjs.headless', 'creepjs.stealth']);
  });

  it('marks a share it cannot read as unknown and omits what the page does not print', () => {
    const findings = judgeCreepjs('0% headless', 'creepjs', 1) ?? [];
    expect(findings.map((finding) => [finding.check, finding.status])).toEqual([
      ['creepjs.headless', 'pass'],
      ['creepjs.stealth', 'unknown'],
    ]);
  });

  it('is unparsed when neither share is there', () => {
    expect(judgeCreepjs('Loading...', 'creepjs', 1)).toBeNull();
    expect(judgeCreepjs('42% like headless', 'creepjs', 1)).toBeNull();
  });
});

describe('a generic page', () => {
  it('applies each rule to the text', () => {
    const findings = judgeGeneric(
      'Your browser looks fine. Trust score: 97',
      [
        { check: 'not_a_bot', pattern: 'you are (a )?bot', expect: 'no_match', hard: true },
        { check: 'trust', pattern: 'trust score: (9\\d|100)', expect: 'match', hard: false },
        { check: 'flagged', pattern: 'looks fine', expect: 'no_match', hard: true },
        { check: 'missing', pattern: 'verified', expect: 'match', hard: false },
        { check: 'broken', pattern: '(', expect: 'match', hard: true },
      ],
      'custom',
      1,
    );
    expect(
      findings.map((finding) => [finding.check, finding.status, finding.hard, finding.observed]),
    ).toEqual([
      ['not_a_bot', 'pass', true, null],
      ['trust', 'pass', false, 'Trust score: 97'],
      ['flagged', 'fail', true, 'looks fine'],
      ['missing', 'fail', false, null],
      ['broken', 'unknown', false, 'the pattern is not a regular expression'],
    ]);
    expect(judgeGeneric('x', [], 'custom', 1)).toEqual([]);
  });
});

describe('runSelftest', () => {
  const loaded = (change: Partial<LoadedPage> = {}): LoadedPage => ({
    httpStatus: 200,
    text: 'page text',
    rows: [],
    published: null,
    screenshot: Buffer.from('png'),
    ...change,
  });

  async function run(
    pages: SelftestPage[] | undefined,
    answers: Record<string, LoadedPage | Error>,
    request: Record<string, unknown> = {},
  ) {
    const root = await mkdtemp(path.join(tmpdir(), 'kage-selftest-'));
    const calls: {
      id: string;
      settleMs: number;
      timeoutMs: number;
      publishedSelector: string | null;
    }[] = [];
    const driver: SelftestDriver = {
      load: (page, options) => {
        calls.push({ id: page.id, ...options });
        const answer = answers[page.id];
        if (answer === undefined || answer instanceof Error) {
          return Promise.reject(answer ?? new Error('no such page'));
        }
        return Promise.resolve(answer);
      },
    };
    let tick = 0;
    const response = await runSelftest(
      driver,
      { ...(pages === undefined ? {} : { pages }), expected, ...request },
      {
        artifacts: new ArtifactWriter(root),
        artifactPrefix: 'selftests/run-1',
        defaultPages: [{ id: 'probe', url: 'https://echo.example.org/probe/', kind: 'probe' }],
        now: () => new Date(Date.UTC(2026, 9, 9, 12, 0, tick++)),
      },
    );
    return { response, calls, root };
  }

  it('loads the configured pages when the request names none', async () => {
    const { response, calls, root } = await run(undefined, {
      probe: loaded({ published: JSON.stringify(report()) }),
    });
    expect(calls).toEqual([
      { id: 'probe', settleMs: 0, timeoutMs: 45_000, publishedSelector: PROBE_REPORT_SELECTOR },
    ]);
    expect(response).toMatchObject({ passed: true, complete: true, score: 100, min_score: 90 });
    expect(response.pages).toEqual([
      {
        id: 'probe',
        url: 'https://echo.example.org/probe/',
        kind: 'probe',
        status: 'ok',
        http_status: 200,
        duration_ms: 1000,
        text: expect.objectContaining({
          path: 'selftests/run-1/probe.txt',
          content_type: 'text/plain; charset=utf-8',
        }) as unknown,
        screenshot: expect.objectContaining({
          path: 'selftests/run-1/probe.png',
          bytes: 3,
        }) as unknown,
      },
    ]);
    // The evidence of a probe is the report it published.
    expect(
      JSON.parse(await readFile(path.join(root, 'selftests/run-1/probe.txt'), 'utf8')),
    ).toMatchObject({ kage_probe: 1 });
    expect(response.started_at).toBe('2026-10-09T12:00:00.000Z');
    expect(new Date(response.finished_at).getTime()).toBeGreaterThan(
      new Date(response.started_at).getTime(),
    );
  });

  it('judges each kind with its judge and keeps going after a page that fails', async () => {
    const pages: SelftestPage[] = [
      { id: 'sanny', url: 'https://bot.sannysoft.com/', kind: 'sannysoft', weight: 3 },
      {
        id: 'creep',
        url: 'https://abrahamjuliot.github.io/creepjs/',
        kind: 'creepjs',
        settle_ms: 100,
      },
      { id: 'echo', url: 'https://echo.example.org/', kind: 'echo' },
      {
        id: 'custom',
        url: 'https://d.example.org/',
        kind: 'generic',
        rules: [{ check: 'ok', pattern: 'page', expect: 'match', hard: false }],
      },
      { id: 'bare', url: 'https://e.example.org/', kind: 'generic' },
      { id: 'down', url: 'https://down.example.org/', kind: 'creepjs' },
      { id: 'changed', url: 'https://changed.example.org/', kind: 'sannysoft' },
      { id: 'probe', url: 'https://echo.example.org/probe/', kind: 'probe' },
    ];
    const { response, calls } = await run(
      pages,
      {
        sanny: loaded({
          rows: [{ cells: ['WebDriver (New)', 'present (failed)'], classes: ['', 'failed'] }],
        }),
        creep: loaded({ text: '0% headless\n0% stealth', screenshot: null }),
        echo: loaded({
          text:
            '{"echo":"kage-echo","http_version":"h2","tls":{"ja4":"x"},"http2":{"akamai":"y","headers":[["user-agent","' +
            LINUX_UA +
            '"]]}}',
        }),
        custom: loaded(),
        bare: loaded(),
        down: new Error(
          'net::ERR_TUNNEL_CONNECTION_FAILED at https://down.example.org/\nCall log: ...',
        ),
        changed: loaded(),
        probe: loaded({ published: null }),
      },
      { min_score: 50, timeout_ms: 5000 },
    );
    expect(calls.map((call) => [call.id, call.settleMs, call.timeoutMs])).toEqual([
      ['sanny', DEFAULT_SETTLE_MS.sannysoft, 5000],
      ['creep', 100, 5000],
      ['echo', 0, 5000],
      ['custom', DEFAULT_SETTLE_MS.generic, 5000],
      ['bare', DEFAULT_SETTLE_MS.generic, 5000],
      ['down', DEFAULT_SETTLE_MS.creepjs, 5000],
      ['changed', DEFAULT_SETTLE_MS.sannysoft, 5000],
      ['probe', 0, 5000],
    ]);
    expect(response.pages.map((page) => [page.id, page.status, page.http_status])).toEqual([
      ['sanny', 'ok', 200],
      ['creep', 'ok', 200],
      ['echo', 'ok', 200],
      ['custom', 'ok', 200],
      ['bare', 'ok', 200],
      ['down', 'unreachable', null],
      ['changed', 'unparsed', 200],
      ['probe', 'unparsed', 200],
    ]);
    expect(response.pages[1]?.screenshot).toBeNull();
    expect(response.pages[5]).toMatchObject({ text: null, screenshot: null });
    const unknowns = response.findings.filter((finding) => finding.status === 'unknown');
    expect(unknowns.map((finding) => [finding.page, finding.check, finding.observed])).toEqual([
      ['down', 'page', 'net::ERR_TUNNEL_CONNECTION_FAILED at https://down.example.org/'],
      ['changed', 'page', 'the page did not look as its kind expects'],
      ['probe', 'page', 'the page did not look as its kind expects'],
    ]);
    // The red WebDriver row is a hard failure: the run fails although the score is above the threshold.
    expect(response.findings.find((finding) => finding.page === 'sanny')).toMatchObject({
      hard: true,
      status: 'fail',
      weight: 3,
    });
    expect(response.score).toBeGreaterThanOrEqual(50);
    expect(response).toMatchObject({ passed: false, complete: false, min_score: 50 });
  });

  it('never fails a persona for an outage of every test site', async () => {
    const { response } = await run(
      [{ id: 'down', url: 'https://down.example.org/', kind: 'creepjs' }],
      {},
    );
    expect(response).toMatchObject({ passed: true, complete: false, score: 100 });
    expect(response.findings).toHaveLength(1);
  });

  it('reports a thrown value that is not an error', async () => {
    const root = await mkdtemp(path.join(tmpdir(), 'kage-selftest-'));
    const response = await runSelftest(
      // eslint-disable-next-line @typescript-eslint/prefer-promise-reject-errors -- a driver may reject with anything
      { load: () => Promise.reject('plain string') },
      { pages: [{ id: 'x', url: 'https://x.example.org/', kind: 'generic' }], expected },
      {
        artifacts: new ArtifactWriter(root),
        artifactPrefix: 'p',
        defaultPages: [],
        now: () => new Date(0),
      },
    );
    expect(response.findings[0]?.observed).toBe('plain string');
  });

  it('is incomplete without any page', async () => {
    const root = await mkdtemp(path.join(tmpdir(), 'kage-selftest-'));
    const response = await runSelftest(
      { load: () => Promise.reject(new Error('unused')) },
      { expected },
      {
        artifacts: new ArtifactWriter(root),
        artifactPrefix: 'p',
        defaultPages: [],
        now: () => new Date(0),
      },
    );
    expect(response).toMatchObject({
      passed: true,
      complete: false,
      score: 100,
      findings: [],
      pages: [],
    });
  });
});
