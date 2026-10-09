import type { OpenContextRequest } from '@kage/contract';
import {
  EngineError,
  formatCpuList,
  keyboardLayoutOf,
  personaCpus,
  type FingerprintDocument,
} from '@kage/engine-core';

import { generatedOf, validateConfig, type CamoufoxConfig } from './fingerprint.js';
import { personaFontconfig } from './fontconfig.js';
import type { HostFacts } from './host.js';

/** Longest value of one `CAMOU_CONFIG_<n>` variable, as the browser reads them. */
const CONFIG_CHUNK = 32_767;

/**
 * Everything a Firefox persona context is launched with, as plain data.
 *
 * Like the Chromium engine's plan it is a pure function of the persisted
 * fingerprint, the open request and the host (spec §6.1 step 3): opening a
 * context generates nothing and asks nobody, so the same document gives the
 * same browser every time. The launcher's own `launchOptions` is not used
 * (ADR 0008, `docs/runbooks/engine-firefox.md`).
 */
export interface LaunchPlan {
  /** The persona's own X display: large enough for its screen and its window. */
  readonly display: { readonly width: number; readonly height: number; readonly depth: number };
  /** The keyboard layout of the display: the one people of the persona's locale type on. */
  readonly layout: string;
  /** The browser's configuration, complete. */
  readonly config: CamoufoxConfig;
  /** The configuration as the browser reads it: `CAMOU_CONFIG_1`, `CAMOU_CONFIG_2`, …. */
  readonly configEnv: Readonly<Record<string, string>>;
  /** The fontconfig file of the claimed system, written into the persona's home before launch. */
  readonly fontconfig: string;
  /** Firefox preferences set in the profile at launch. */
  readonly preferences: Readonly<Record<string, string | number | boolean>>;
  /** CPUs the browser may run on. */
  readonly cpus: readonly number[];
  readonly proxy: {
    readonly server: string;
    readonly username: string;
    readonly password: string;
  };
}

/**
 * Preferences every persona runs with.
 *
 * - Loopback is sent through the proxy like any other host: a page that
 *   asks for `localhost` must not reach the sidecar's own network (§19).
 * - WebRTC gathers nothing that would leave around the proxy; the build's
 *   default says the same and is repeated here so that a build that
 *   changes it fails a test instead of leaking.
 * - Printing goes to a file without a dialog: that is how a PDF is made,
 *   by the keyboard shortcut a person would use.
 */
export function basePreferences(pdfPath: string): Record<string, string | number | boolean> {
  const printer = 'print.printer_Mozilla_Save_to_PDF';
  return {
    'webgl.force-enabled': true,
    'network.proxy.allow_hijacking_localhost': true,
    'network.proxy.no_proxies_on': '',
    'media.peerconnection.ice.proxy_only_if_behind_proxy': true,
    'media.peerconnection.ice.default_address_only': true,
    'network.http.http3.enable': false,
    'print.always_print_silent': true,
    print_printer: 'Mozilla Save to PDF',
    [`${printer}.print_to_file`]: true,
    [`${printer}.print_to_filename`]: pdfPath,
    [`${printer}.print_bgcolor`]: true,
    [`${printer}.print_bgimages`]: true,
  };
}

/** Splits a configuration into the environment variables the browser reads it from. */
export function configEnvironment(config: CamoufoxConfig): Record<string, string> {
  const text = JSON.stringify(config);
  const env: Record<string, string> = {};
  for (let offset = 0; offset < text.length; offset += CONFIG_CHUNK) {
    env[`CAMOU_CONFIG_${Math.floor(offset / CONFIG_CHUNK) + 1}`] = text.slice(
      offset,
      offset + CONFIG_CHUNK,
    );
  }
  return env;
}

/**
 * The keys an open request adds to the persisted configuration: language,
 * time zone and position always come from Kage, never from a lookup of the
 * proxy's address (the launcher's `geoip` is not used).
 *
 * @throws {EngineError} `invalid_request` when the locale has no region.
 */
export function localeConfig(request: OpenContextRequest): CamoufoxConfig {
  const [language, ...rest] = request.locale.split('-');
  const region = rest.find((part) => /^([A-Za-z]{2}|\d{3})$/.test(part));
  const script = rest.find((part) => /^[A-Za-z]{4}$/.test(part));
  if (language === undefined || region === undefined) {
    throw new EngineError(
      'invalid_request',
      `the firefox engine needs a locale with a region, for example de-DE; got ${request.locale}`,
    );
  }
  const config: CamoufoxConfig = {
    'locale:language': language.toLowerCase(),
    'locale:region': region.toUpperCase(),
    'locale:all': request.languages.join(', '),
    timezone: request.timezone,
  };
  if (script !== undefined) {
    config['locale:script'] = script;
  }
  if (request.geolocation !== null) {
    config['geolocation:latitude'] = request.geolocation.latitude;
    config['geolocation:longitude'] = request.geolocation.longitude;
    config['geolocation:accuracy'] = request.geolocation.accuracy_m;
  }
  return config;
}

/**
 * Builds the launch plan of a context.
 *
 * @param pdfPath where the browser writes a printed page.
 * @throws {EngineError} `invalid_request` when the request's screen is not
 *   the fingerprint's: a persona's screen never changes.
 */
export function launchPlan(
  document: FingerprintDocument,
  request: OpenContextRequest,
  host: Pick<HostFacts, 'installDir' | 'properties' | 'fontconfig' | 'cpus'>,
  pdfPath: string,
): LaunchPlan {
  const { screen } = document.native;
  const asked = request.screen;
  if (
    asked.width !== screen.width ||
    asked.height !== screen.height ||
    asked.dpr !== screen.dpr ||
    asked.color_depth !== screen.color_depth
  ) {
    throw new EngineError(
      'invalid_request',
      `screen ${asked.width}x${asked.height}@${asked.dpr}/${asked.color_depth} is not the fingerprint's ${screen.width}x${screen.height}@${screen.dpr}/${screen.color_depth}`,
    );
  }
  if (host.cpus.length === 0) {
    throw new EngineError('internal', 'the host reports no usable CPU');
  }
  const generated = generatedOf(document);
  const config: CamoufoxConfig = { ...generated.config, ...localeConfig(request) };
  validateConfig(config, host.properties);
  const side = (key: string, fallback: number) => {
    const value = config[key];
    return typeof value === 'number' ? Math.max(fallback, value) : fallback;
  };
  return {
    // A maximized window on Windows is a few pixels larger than its screen;
    // the display holds whichever is larger, so nothing of the window is cut off.
    display: {
      width: side('window.outerWidth', screen.width),
      height: side('window.outerHeight', screen.height),
      depth: 24,
    },
    layout: keyboardLayoutOf(request.locale),
    config,
    configEnv: configEnvironment(config),
    fontconfig: personaFontconfig(
      document.claimed_os,
      host.fontconfig[document.claimed_os],
      host.installDir,
    ),
    preferences: { ...basePreferences(pdfPath), 'webgl.enable-webgl2': generated.webgl2 },
    cpus: personaCpus(document.seed, document.native.hardware_concurrency, host.cpus),
    proxy: {
      server: request.proxy.server,
      username: request.proxy.username,
      password: request.proxy.password,
    },
  };
}

/** The value of `KAGE_CPU_LIST` for a plan. */
export function cpuListOf(plan: LaunchPlan): string {
  return formatCpuList(plan.cpus);
}
