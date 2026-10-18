import { mkdir, writeFile } from 'node:fs/promises';
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
  DisplayPool,
  EngineError,
  majorOfVersion,
  parseFingerprint,
  startXvfb,
  summarize,
  type Engine,
  type EngineContext,
  type EngineOptions,
  type EngineStatus,
  type VirtualDisplay,
} from '@kage/engine-core';
import { firefox, type BrowserContext } from 'playwright-core';

import { FirefoxContext } from './context.js';
import { generateFingerprint, upgradeFingerprint } from './fingerprint.js';
import type { HostFacts } from './host.js';
import { cpuListOf, launchPlan } from './launch-plan.js';
import { keepBeforeUpgrade } from './upgrade.js';

/** The wrapper that starts the browser under the persona's CPU affinity mask. */
export const CAMOUFOX_WRAPPER = fileURLToPath(
  new URL('../bin/camoufox-affinity.sh', import.meta.url),
);

/** How long the browser may take to start. */
const LAUNCH_TIMEOUT_MS = 90_000;

/** Directories of one persona below its profile directory. */
export interface ProfilePaths {
  /** The Firefox profile: cookies, storage, history, cache. */
  readonly userData: string;
  /** `HOME` of the browser process. */
  readonly home: string;
  /** The directory with the persona's fontconfig file. */
  readonly fontconfig: string;
  /** Where the browser writes a printed page. */
  readonly pdf: string;
}

/**
 * Where the parts of a persona's profile live.
 *
 * A profile directory may also hold an `omni.sha256` file: profiles that
 * ran the Camoufox 152 build with the Juggler backport (ADR 0010) noted
 * there which archive they last started under. Nothing reads it any more.
 */
export function profilePaths(profileDir: string): ProfilePaths {
  const home = path.join(profileDir, 'home');
  return {
    userData: path.join(profileDir, 'user-data'),
    home,
    fontconfig: path.join(home, 'fontconfig'),
    pdf: path.join(home, 'print', 'page.pdf'),
  };
}

/**
 * The Firefox engine: Camoufox, a Firefox whose fingerprint is set inside
 * the browser, driven through its own playwright-core, one browser process
 * per persona context on the persona's own X display (spec §10.3).
 *
 * The launcher (camoufox-js) supplies the real-device fingerprints and the
 * tables that go with them; how a browser is started from a persisted
 * fingerprint is this engine's own, so that it is the same every time.
 * Input is never the driver's: the shared behavior engine uses the
 * display's mouse and keyboard, and the build's own `humanize` stays off.
 */
export class FirefoxEngine implements Engine {
  readonly name = 'firefox' as const;
  private readonly displays = new DisplayPool();
  private readonly open = new Set<EngineContext>();

  constructor(
    private readonly options: EngineOptions,
    private readonly host: HostFacts,
  ) {}

  status(): EngineStatus {
    return { browserVersion: this.host.version };
  }

  generatePersona(request: GeneratePersonaRequest): Promise<GeneratePersonaResponse> {
    try {
      const document = generateFingerprint(request, this.host);
      return Promise.resolve({ fingerprint: document, summary: summarize(document) });
    } catch (error) {
      return Promise.reject(error instanceof Error ? error : new Error(String(error)));
    }
  }

  upgradePersona(request: UpgradePersonaRequest): Promise<UpgradePersonaResponse> {
    try {
      const { document, upgraded } = upgradeFingerprint(
        parseFingerprint(request.fingerprint),
        this.host.version,
      );
      return Promise.resolve({ fingerprint: document, summary: summarize(document), upgraded });
    } catch (error) {
      return Promise.reject(error instanceof Error ? error : new Error(String(error)));
    }
  }

  async openContext(request: OpenContextRequest): Promise<EngineContext> {
    const document = parseFingerprint(request.fingerprint);
    if (document.engine !== 'firefox') {
      throw new EngineError(
        'invalid_request',
        `a ${document.engine} fingerprint cannot open a firefox context`,
      );
    }
    if (majorOfVersion(document.browser_version) !== majorOfVersion(this.host.version)) {
      throw new EngineError(
        'browser_major_mismatch',
        `the fingerprint is of Firefox ${document.browser_version}, the engine runs ${this.host.version}: move it with POST /v1/personas/upgrade first`,
      );
    }
    const profileDir = path.join(this.options.personasDir, ...request.profile_dir.split('/'));
    const paths = profilePaths(profileDir);
    const plan = launchPlan(document, request, this.host, paths.pdf);
    // The first start under a newer Firefox moves the profile for good: the
    // profile as it was is kept first, or the browser does not start.
    const kept = await keepBeforeUpgrade({
      personasDir: this.options.personasDir,
      personaId: request.persona_id,
      profileDir,
      relativeProfileDir: request.profile_dir,
      userData: paths.userData,
      hostVersion: this.host.version,
      now: new Date(),
    }).catch((error: unknown) => {
      throw new EngineError(
        'browser_launch_failed',
        `the profile could not be kept before its first start under Firefox ${this.host.version}: ${error instanceof Error ? error.message : String(error)}`,
        { cause: error },
      );
    });
    if (kept !== null) {
      this.options.log.info(
        { persona_id: request.persona_id, kept },
        'the profile starts under a newer Firefox: the profile as it was is kept',
      );
    }
    for (const directory of [paths.userData, paths.fontconfig, path.dirname(paths.pdf)]) {
      await mkdir(directory, { recursive: true, mode: 0o700 });
    }
    await writeFile(path.join(paths.fontconfig, 'fonts.conf'), plan.fontconfig, { mode: 0o600 });

    const displayNumber = this.displays.acquire();
    let display: VirtualDisplay | null = null;
    let browser: BrowserContext | null = null;
    const release = async () => {
      await display?.stop();
      this.displays.release(displayNumber);
    };
    try {
      display = await startXvfb(displayNumber, plan.display, { layout: plan.layout });
      browser = await firefox.launchPersistentContext(paths.userData, {
        executablePath: CAMOUFOX_WRAPPER,
        headless: false,
        // No viewport override: the window is the fingerprint's.
        viewport: null,
        timeout: LAUNCH_TIMEOUT_MS,
        firefoxUserPrefs: { ...plan.preferences },
        proxy: plan.proxy,
        // The browser's whole environment. Nothing of the sidecar's own
        // (its token least of all) reaches the browser process.
        env: {
          PATH: process.env['PATH'] ?? '/usr/local/bin:/usr/bin:/bin',
          HOME: paths.home,
          XDG_CONFIG_HOME: path.join(paths.home, '.config'),
          XDG_CACHE_HOME: path.join(paths.home, '.cache'),
          DISPLAY: display.name,
          FONTCONFIG_PATH: paths.fontconfig,
          KAGE_CAMOUFOX: this.host.binary,
          KAGE_CPU_LIST: cpuListOf(plan),
          ...(this.options.audioServer === undefined
            ? {}
            : { PULSE_SERVER: this.options.audioServer }),
          ...plan.configEnv,
        },
      });
      const page = browser.pages()[0] ?? (await browser.newPage());
      const applied: AppliedIdentity = {
        claimed_os: document.claimed_os,
        user_agent: document.user_agent,
        screen: document.native.screen,
        timezone: request.timezone,
        languages: [...request.languages],
        hardware_concurrency: document.native.hardware_concurrency,
        // Pixels are Mesa's; the strings a page reads are the fingerprint's.
        gl: 'llvmpipe',
        inject: 'none',
        script_sha256: null,
      };
      const context: EngineContext = new FirefoxContext({
        context: browser,
        page,
        browserVersion: this.host.version,
        applied,
        personaSeed: document.seed,
        display: display.name,
        pdfPath: paths.pdf,
        selftestPages: this.options.selftestPages,
        log: this.options.log,
        release: async () => {
          this.open.delete(context);
          await release();
        },
      });
      this.open.add(context);
      this.options.log.info(
        { persona_id: request.persona_id, display: display.name, cpus: cpuListOf(plan) },
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
      throw new EngineError('browser_launch_failed', `Camoufox did not start: ${reason}`, {
        cause: error,
      });
    }
  }

  async close(): Promise<void> {
    await Promise.all([...this.open].map((context) => context.close()));
  }
}
