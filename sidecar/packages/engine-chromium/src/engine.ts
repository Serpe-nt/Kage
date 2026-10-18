import path from 'node:path';
import { fileURLToPath } from 'node:url';

import type {
  AppliedIdentity,
  GeneratePersonaRequest,
  GeneratePersonaResponse,
  OpenContextRequest,
  UpgradePersonaRequest,
  UpgradePersonaResponse,
} from '@kage/contract';
import {
  EngineError,
  majorOfVersion,
  moveToVersion,
  DisplayPool,
  parseFingerprint,
  startXvfb,
  summarize,
  type Engine,
  type EngineContext,
  type EngineOptions,
  type EngineStatus,
  type VirtualDisplay,
} from '@kage/engine-core';
import { chromium, type BrowserContext } from 'patchright';

import { ChromiumContext } from './context.js';
import {
  buildInjection,
  chromeUserAgentOf,
  generateFingerprint,
  type Injection,
} from './fingerprint.js';
import type { HostFacts } from './host.js';
import { cpuListOf, launchPlan } from './launch-plan.js';
import { PageReader } from './page-reader.js';
import { prepareProfile } from './profile.js';

/** The wrapper that starts Chrome under the persona's CPU affinity mask. */
export const CHROME_WRAPPER = fileURLToPath(new URL('../bin/chrome-affinity.sh', import.meta.url));

/** How long Chrome may take to start. */
const LAUNCH_TIMEOUT_MS = 60_000;

/**
 * The Chromium engine: patchright driving the real, installed Chrome, one
 * browser process per persona context (spec §10.3).
 *
 * What it does on purpose, following patchright's guidance and ADR 0009:
 * no user agent through driver options, no init script through the driver
 * (it would switch the HTTP cache off), no route, nothing that needs the
 * `Runtime` or `Console` debugger domains, no read of a page through the
 * driver (it marks every evaluation as a user gesture; see
 * `page-reader.ts`), headful on the persona's own X display, and Chrome's
 * sandbox on.
 */
export class ChromiumEngine implements Engine {
  readonly name = 'chromium' as const;
  private readonly displays = new DisplayPool();
  private readonly open = new Set<EngineContext>();

  constructor(
    private readonly options: EngineOptions,
    private readonly host: HostFacts,
  ) {}

  status(): EngineStatus {
    return { browserVersion: this.host.chromeVersion };
  }

  generatePersona(request: GeneratePersonaRequest): Promise<GeneratePersonaResponse> {
    try {
      const document = generateFingerprint(request, {
        browserVersion: this.host.chromeVersion,
        allowCrossOsClaims: this.options.allowCrossOsClaims,
      });
      return Promise.resolve({ fingerprint: document, summary: summarize(document) });
    } catch (error) {
      return Promise.reject(error instanceof Error ? error : new Error(String(error)));
    }
  }

  upgradePersona(request: UpgradePersonaRequest): Promise<UpgradePersonaResponse> {
    try {
      const stored = parseFingerprint(request.fingerprint);
      if (stored.engine !== 'chromium') {
        throw new EngineError(
          'invalid_request',
          `a ${stored.engine} fingerprint is not moved by the chromium engine`,
        );
      }
      const { document, upgraded } = moveToVersion(
        stored,
        this.host.chromeVersion,
        chromeUserAgentOf,
      );
      return Promise.resolve({ fingerprint: document, summary: summarize(document), upgraded });
    } catch (error) {
      return Promise.reject(error instanceof Error ? error : new Error(String(error)));
    }
  }

  async openContext(request: OpenContextRequest): Promise<EngineContext> {
    const document = parseFingerprint(request.fingerprint);
    if (document.engine !== 'chromium') {
      throw new EngineError(
        'invalid_request',
        `a ${document.engine} fingerprint cannot open a chromium context`,
      );
    }
    if (document.claimed_os !== 'linux' && !this.options.allowCrossOsClaims) {
      throw new EngineError(
        'cross_os_claim_disabled',
        `the chromium engine claims linux only; this fingerprint claims ${document.claimed_os} (ADR 0009)`,
      );
    }
    const installed = majorOfVersion(this.host.chromeVersion);
    if (majorOfVersion(document.browser_version) !== installed) {
      throw new EngineError(
        'browser_major_mismatch',
        `the fingerprint was generated for Chrome ${document.browser_version}, the engine runs ${this.host.chromeVersion}: move it with POST /v1/personas/upgrade first`,
      );
    }
    const plan = launchPlan(document, request, { cpus: this.host.cpus, gl: this.options.gl });
    const injection: Injection | null = plan.inject === 'window' ? buildInjection(document) : null;
    const profileDir = path.join(this.options.personasDir, ...request.profile_dir.split('/'));
    const paths = await prepareProfile(profileDir, plan.preferences);

    const displayNumber = this.displays.acquire();
    let display: VirtualDisplay | null = null;
    let browser: BrowserContext | null = null;
    const release = async () => {
      await display?.stop();
      this.displays.release(displayNumber);
    };
    try {
      display = await startXvfb(displayNumber, plan.display, {
        layout: plan.layout,
        workArea: plan.workArea,
      });
      browser = await chromium.launchPersistentContext(paths.userData, {
        // The real Chrome of the image (the driver's `chrome` channel),
        // started through a wrapper that only sets the CPU affinity mask.
        executablePath: CHROME_WRAPPER,
        headless: false,
        // No viewport override: the window is the persona's screen.
        viewport: null,
        chromiumSandbox: true,
        timeout: LAUNCH_TIMEOUT_MS,
        args: [...plan.args],
        proxy: plan.proxy,
        ...(plan.geolocation === null ? {} : { geolocation: plan.geolocation }),
        // The browser's whole environment. Nothing of the sidecar's own
        // (its token least of all) reaches the browser process.
        env: {
          PATH: process.env['PATH'] ?? '/usr/local/bin:/usr/bin:/bin',
          HOME: paths.home,
          XDG_CONFIG_HOME: path.join(paths.home, '.config'),
          XDG_CACHE_HOME: path.join(paths.home, '.cache'),
          DISPLAY: display.name,
          KAGE_CHROME: this.host.chromePath,
          KAGE_CPU_LIST: cpuListOf(plan),
          ...(this.options.audioServer === undefined
            ? {}
            : { PULSE_SERVER: this.options.audioServer }),
          ...plan.env,
        },
      });
      const page = browser.pages()[0] ?? (await browser.newPage());
      const session = await browser.newCDPSession(page);
      const owner = browser.browser();
      if (owner === null) {
        throw new Error('the driver gave no browser for the persistent context');
      }
      const browserSession = await owner.newBrowserCDPSession();
      const version = await browserSession.send('Browser.getVersion');
      // Log entries and the protocol of responses: both are read from the
      // browser process and invisible to pages.
      await session.send('Log.enable');
      await session.send('Network.enable');
      // Before the first navigation: every document gets the reader's world.
      const reader = await PageReader.attach(session);
      if (injection !== null) {
        await session.send('Emulation.setUserAgentOverride', injection.userAgentOverride);
        await session.send('Page.enable');
        await session.send('Page.addScriptToEvaluateOnNewDocument', { source: injection.script });
      }
      const applied: AppliedIdentity = {
        claimed_os: document.claimed_os,
        user_agent: injection === null ? version.userAgent : injection.userAgentOverride.userAgent,
        screen: document.native.screen,
        timezone: request.timezone,
        languages: [...request.languages],
        hardware_concurrency: plan.cpus.length,
        gl: plan.gl,
        inject: plan.inject,
        script_sha256: injection?.sha256 ?? null,
      };
      const context: EngineContext = new ChromiumContext({
        context: browser,
        page,
        session,
        reader,
        browserSession,
        browserVersion: version.product.replace(/^[^/]*\//, ''),
        applied,
        personaSeed: document.seed,
        display: display.name,
        windowOrigin: { x: plan.window.x, y: plan.window.y },
        selftestPages: this.options.selftestPages,
        log: this.options.log,
        release: async () => {
          this.open.delete(context);
          await release();
        },
      });
      this.open.add(context);
      this.options.log.info(
        {
          persona_id: request.persona_id,
          display: display.name,
          cpus: cpuListOf(plan),
          inject: plan.inject,
        },
        'context opened',
      );
      return context;
    } catch (error) {
      await browser?.close().catch(() => undefined);
      await release();
      if (error instanceof EngineError) {
        throw error;
      }
      // The proxy password is part of the launch options; the driver's
      // message may quote them, so only its first line is kept.
      const reason = (error instanceof Error ? error.message : String(error)).split('\n')[0] ?? '';
      throw new EngineError('browser_launch_failed', `Chrome did not start: ${reason}`, {
        cause: error,
      });
    }
  }

  async close(): Promise<void> {
    await Promise.all([...this.open].map((context) => context.close()));
  }
}
