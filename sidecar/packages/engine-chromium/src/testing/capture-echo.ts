import { execFile } from 'node:child_process';
import { mkdir, mkdtemp } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { promisify } from 'node:util';

import type { NavigateRequest, PageResult } from '@kage/contract';
import { ArtifactWriter, silentLog, type FingerprintDocument } from '@kage/engine-core';

import { ChromiumEngine } from '../engine.js';
import { detectHost } from '../host.js';
import { profilePaths } from '../profile.js';

import { startTestProxy } from '@kage/engine-core/testing';

const run = promisify(execFile);

/** One request as the fingerprint echo saw it. */
export interface CapturedRequest {
  readonly method: string;
  readonly http_version: string;
  /** Header lines in wire order, as the browser spelled them. */
  readonly headers: readonly (readonly [string, string])[];
}

/**
 * What the engine's real browser showed the fingerprint echo: the file
 * `tests/fixtures/fingerprints/browser/chromium-<major>.json`
 * (`kage_http::selftest::BrowserCapture`).
 */
export interface EchoCapture {
  readonly engine: 'chromium';
  readonly browser_version: string;
  readonly os: 'linux';
  /** The day of the capture, `YYYY-MM-DD`. */
  readonly recorded: string;
  readonly ja4: string;
  readonly ja4_r: string;
  readonly akamai: string;
  /** Header names of a typed navigation over HTTP/2, in wire order. */
  readonly headers: readonly string[];
  readonly requests: Readonly<Record<string, CapturedRequest>>;
}

/** Where the echo runs and how the browser comes to trust it. */
export interface CaptureOptions {
  /** The DNS name in the echo's certificate. A name, never an address: JA4 records whether one was sent. */
  readonly echoHost: string;
  /** The port on loopback the echo listens on. */
  readonly echoPort: number;
  /** PEM file of the authority that signed the echo's certificate. */
  readonly caFile: string;
  /** The day to record; default today in UTC. */
  readonly recorded?: string;
}

/** An answer of `kage echo serve`, as far as the capture reads it. */
interface Observation {
  readonly method: string;
  readonly http_version: string;
  readonly tls: { readonly ja4: string; readonly ja4_r: string };
  readonly http2?: { readonly akamai: string; readonly headers: [string, string][] } | null;
  readonly http1?: { readonly headers: [string, string][] } | null;
}

const SEED = 'ca97c2e04f1b3d5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2';
const DONE = '#kage-echo-capture[data-done]';

const request = (observation: Observation): CapturedRequest => ({
  method: observation.method,
  http_version: observation.http_version,
  headers: observation.http2?.headers ?? observation.http1?.headers ?? [],
});

function published(result: PageResult): Record<string, Observation | null> {
  const json =
    /<script type="application\/json" id="kage-echo-capture"[^>]*>([\s\S]*?)<\/script>/.exec(
      result.dom_html ?? '',
    )?.[1];
  if (result.error !== null || json === undefined) {
    throw new Error(
      `the echo's capture page did not publish: ${result.error?.message ?? 'no capture element in the document'}`,
    );
  }
  return JSON.parse(json) as Record<string, Observation | null>;
}

/**
 * Gives a persona's browser a certificate database that trusts one more
 * authority, the way a person adds one: Chrome on Linux reads user-trusted
 * authorities from the NSS database in its home directory. No flag that
 * weakens certificate checks is involved.
 */
async function trustAuthority(home: string, caFile: string): Promise<void> {
  const database = path.join(home, '.pki', 'nssdb');
  await mkdir(database, { recursive: true, mode: 0o700 });
  await run('certutil', ['-d', `sql:${database}`, '-N', '--empty-password']);
  await run('certutil', [
    '-d',
    `sql:${database}`,
    '-A',
    '-t',
    'C,,',
    '-n',
    'kage-fingerprint-echo',
    '-i',
    caFile,
  ]);
}

/**
 * Points the engine's real browser at the in-repo fingerprint echo, the way
 * a persona context runs in production (same launch plan, through an
 * authenticated proxy), and records what the echo saw: the TLS and HTTP/2
 * fingerprints and the headers of a typed navigation, `fetch()`, a JSON
 * POST, `XMLHttpRequest` and a navigation over HTTP/1.1 (follow-up F5). A
 * followed link is not captured yet: following one takes a click, which
 * arrives with the behavior engine.
 *
 * @throws when the browser cannot reach or trust the echo.
 */
export async function captureEcho(options: CaptureOptions): Promise<EchoCapture> {
  const origin = `https://${options.echoHost}:${options.echoPort}`;
  const http1 = `https://h1.${options.echoHost}:${options.echoPort}`;
  // The only thing the test proxy relays is the echo, by its names.
  const proxy = await startTestProxy({
    httpPort: 1,
    tunnels: {
      [`${options.echoHost}:${options.echoPort}`]: options.echoPort,
      [`h1.${options.echoHost}:${options.echoPort}`]: options.echoPort,
    },
  });
  const root = await mkdtemp(path.join(tmpdir(), 'kage-capture-'));
  const host = await detectHost();
  const engine = new ChromiumEngine(
    {
      personasDir: path.join(root, 'personas'),
      allowCrossOsClaims: false,
      gl: 'llvmpipe',
      selftestPages: [],
      log: silentLog,
    },
    host,
  );
  try {
    const { fingerprint } = await engine.generatePersona({
      seed: SEED,
      claimed_os: 'linux',
      languages: ['en-US', 'en'],
      screen: { min_width: 1920, max_width: 1920, min_height: 1080, max_height: 1080 },
    });
    const document = fingerprint as FingerprintDocument;
    await trustAuthority(profilePaths(path.join(root, 'personas', 'capture')).home, options.caFile);
    const context = await engine.openContext({
      persona_id: '0199c1f2-6b7a-7c3e-9d41-00000000cafe',
      fingerprint: document,
      proxy: { server: proxy.server, username: proxy.username, password: proxy.password },
      locale: 'en-US',
      languages: ['en-US', 'en'],
      timezone: 'UTC',
      geolocation: null,
      screen: document.native.screen,
      profile_dir: 'capture',
    });
    const env = {
      artifacts: new ArtifactWriter(path.join(root, 'artifacts')),
      defaultArtifactPrefix: 'capture',
    };
    const navigate = (url: string, change: Partial<NavigateRequest> = {}) =>
      context.navigate(
        {
          url,
          wait: { until: 'load' },
          capture: { dom: true, raw: true, xhr: null, console: false, screenshot: null, pdf: null },
          timeout_ms: 30_000,
          ...change,
        },
        env,
      );
    try {
      // The first request of a fresh browser: a typed navigation, with the
      // page's own fetch() and XMLHttpRequest calls after it.
      const typed = published(
        await navigate(`${origin}/browser`, { wait: { until: 'load', selector: DONE } }),
      );
      const plain = await navigate(`${http1}/`);
      if (plain.error !== null || plain.raw_body === null) {
        throw new Error(
          `the echo's HTTP/1.1 name did not answer: ${plain.error?.message ?? 'no body'}`,
        );
      }
      const overHttp1 = JSON.parse(plain.raw_body.data) as Observation;

      const navigation = typed['navigation'];
      if (navigation?.http2 == null) {
        throw new Error('the typed navigation did not arrive over HTTP/2');
      }
      const requests: Record<string, CapturedRequest> = { navigation: request(navigation) };
      for (const [name, observation] of [
        ['fetch_get', typed['fetch_get']],
        ['fetch_post_json', typed['fetch_post_json']],
        ['xhr_get', typed['xhr_get']],
        ['navigation_http1', overHttp1],
      ] as const) {
        if (observation == null) {
          throw new Error(`the capture has no ${name}: the page could not ask the echo`);
        }
        requests[name] = request(observation);
      }
      return {
        engine: 'chromium',
        browser_version: context.browserVersion,
        os: 'linux',
        recorded: options.recorded ?? new Date().toISOString().slice(0, 10),
        ja4: navigation.tls.ja4,
        ja4_r: navigation.tls.ja4_r,
        akamai: navigation.http2.akamai,
        headers: navigation.http2.headers.map(([name]) => name),
        requests,
      };
    } finally {
      await context.close();
    }
  } finally {
    await engine.close();
    await proxy.stop();
  }
}

/** The fixture file content: pretty JSON with a final newline. */
export function serializeCapture(capture: EchoCapture): string {
  return `${JSON.stringify(capture, null, 2)}\n`;
}

/** The fixture file name of a capture: `chromium-155.json`. */
export function captureFileName(capture: EchoCapture): string {
  return `${capture.engine}-${capture.browser_version.split('.')[0] ?? 'unknown'}.json`;
}
