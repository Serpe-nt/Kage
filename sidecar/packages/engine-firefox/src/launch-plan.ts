import type { ClaimedOs, OpenContextRequest } from '@kage/contract';
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

/** Longest value of one `CAMOU_CONFIG_<n>` or `CAMOU_PREFS_<n>` variable, as the browser reads them. */
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
  /** The browser's configuration as it is sent: what the build reads of the document, and the request's. */
  readonly config: CamoufoxConfig;
  /**
   * The browser's environment from the plan: the configuration as
   * `CAMOU_CONFIG_1`, `CAMOU_CONFIG_2`, …, and the preferences as
   * `CAMOU_PREFS_1`, …, which the build applies while it starts.
   */
  readonly configEnv: Readonly<Record<string, string>>;
  /** The fontconfig file of the claimed system, written into the persona's home before launch. */
  readonly fontconfig: string;
  /**
   * Firefox preferences of the launch. The build applies them while it
   * starts (through `CAMOU_PREFS`), and the driver sets them again once it
   * is connected: a preference Gecko reads during startup, the language of
   * the browser's own messages above all, would otherwise come too late.
   */
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
 * Configuration keys the engine never sends, whatever a document holds.
 *
 * `navigator.doNotTrack` and `navigator.globalPrivacyControl` are Firefox
 * preferences, and a page reads them twice: in a script, and in the `DNT`
 * and `Sec-GPC` headers of every request. The real-device records of the
 * generator say `"1"` and `true` for most personas (all of 400 drawn, and
 * two in three Windows and nine in ten macOS ones), where a Firefox nobody
 * configured says `unspecified` and `false`. Camoufox 152 ignored both
 * keys; 156 applies the second to the script value alone, so that a page
 * read `true` with no header (measured). Without the keys the persona is
 * Firefox's default in both places.
 */
export const NEVER_APPLIED: readonly string[] = [
  'navigator.doNotTrack',
  'navigator.globalPrivacyControl',
];

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
 * - Sound waits for the user, as in a Firefox nobody configured
 *   (`media.autoplay.default` 1). The 156 build ships that default itself;
 *   the 152 build allowed every page to play at once, which a page read
 *   with `navigator.getAutoplayPolicy()` (M5.1). The line stays as a guard
 *   against a build that changes it again.
 * - The memory cache and compression dictionaries are Firefox's defaults.
 *   The build turns both off (`browser.cache.memory.enable` in its
 *   `camoufox.cfg`, `network.http.dictionaries.enable` in its compiled
 *   defaults), so that a page would time repeated loads from disk and a
 *   site that offers a dictionary would never be told one is available,
 *   where a stock Firefox 156 does both (the audit of the 156 milestone).
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
    'media.autoplay.default': 1,
    'browser.cache.memory.enable': true,
    'network.http.dictionaries.enable': true,
    'print.always_print_silent': true,
    print_printer: 'Mozilla Save to PDF',
    [`${printer}.print_to_file`]: true,
    [`${printer}.print_to_filename`]: pdfPath,
    [`${printer}.print_bgcolor`]: true,
    [`${printer}.print_bgimages`]: true,
  };
}

/**
 * The language of the browser's own interface (its messages, form
 * validation, error pages) by the persona's locale: the Firefox build a
 * person there installs. Keys are a locale tag or a language; the tag is
 * tried first. Covers every locale the persona tables give
 * (`kage_core::persona::locale`, D34).
 */
const FIREFOX_UI_LOCALES: Readonly<Record<string, string>> = {
  de: 'de',
  fr: 'fr',
  it: 'it',
  nl: 'nl',
  pl: 'pl',
  ro: 'ro',
  sv: 'sv-SE',
  pt: 'pt-PT',
  'pt-BR': 'pt-BR',
  es: 'es-ES',
  'es-AR': 'es-AR',
  'es-CL': 'es-CL',
  'es-MX': 'es-MX',
  en: 'en-US',
  'en-GB': 'en-GB',
  // Ireland has no Firefox of its own in English; people there use the British one.
  'en-IE': 'en-GB',
  'en-CA': 'en-CA',
};

/**
 * The interface language of a persona's Firefox (`intl.locale.requested`),
 * one the build ships, else `en-US`.
 *
 * Measured on Mozilla's own 156.0.1 builds: with the system's locale of the
 * country, the German build reports `de-DE` to `Intl` and German messages,
 * the French `fr-FR`, the Italian `it-IT`, the British `en-GB`. Camoufox's
 * locale keys give `Intl` the persona's own tag; this gives it the
 * messages. Camoufox 152 shipped `en-US` alone, so every persona's form
 * messages were English.
 */
export function firefoxUiLocale(locale: string, available: readonly string[]): string {
  const [language = '', ...rest] = locale.split('-');
  const region = rest.find((part) => /^([A-Za-z]{2}|\d{3})$/.test(part));
  const tag = region === undefined ? language : `${language.toLowerCase()}-${region.toUpperCase()}`;
  const wanted = FIREFOX_UI_LOCALES[tag] ?? FIREFOX_UI_LOCALES[language.toLowerCase()];
  return wanted !== undefined && available.includes(wanted) ? wanted : 'en-US';
}

/** Whether a configuration names an Apple graphics processor. */
const APPLE_GPU = /\bApple\b/;

/**
 * WebGPU as Firefox 156 has it on the claimed device.
 *
 * Firefox enables WebGPU on Windows and on Macs with Apple's graphics
 * processors, and not on Linux or an Intel Mac (`dom.webgpu.enabled`), so
 * a persona claiming Windows without `navigator.gpu` was one more thing a
 * page could read. The persona's device is not the host's, so its adapter
 * is blocked (`gfx.blacklist.webgpu` 4, a device Firefox refuses):
 * `requestAdapter()` answers `null`, as on a machine whose graphics driver
 * Firefox does not trust (measured: present, `null`, nothing else changes).
 */
export function webgpuPreferences(
  os: ClaimedOs,
  config: CamoufoxConfig,
): Record<string, string | number | boolean> {
  const text = (key: string) => {
    const value = config[key];
    return typeof value === 'string' ? value : '';
  };
  const gpu = `${text('webGl:vendor')} ${text('webGl:renderer')}`;
  if (os === 'windows' || (os === 'macos' && APPLE_GPU.test(gpu))) {
    return { 'dom.webgpu.enabled': true, 'gfx.blacklist.webgpu': 4 };
  }
  return {};
}

/** Splits a text into the chunks of one variable family (`<prefix>1`, `<prefix>2`, …). */
function chunked(prefix: string, text: string): Record<string, string> {
  const env: Record<string, string> = {};
  for (let offset = 0; offset < text.length; offset += CONFIG_CHUNK) {
    env[`${prefix}${Math.floor(offset / CONFIG_CHUNK) + 1}`] = text.slice(
      offset,
      offset + CONFIG_CHUNK,
    );
  }
  return env;
}

/** Splits a configuration into the environment variables the browser reads it from. */
export function configEnvironment(config: CamoufoxConfig): Record<string, string> {
  return chunked('CAMOU_CONFIG_', JSON.stringify(config));
}

/**
 * The preferences as the build's `camoufox.cfg` reads them while the
 * browser starts (`CAMOU_PREFS_<n>`, JSON): it writes each to the default
 * branch before anything reads it.
 */
export function preferenceEnvironment(
  preferences: Readonly<Record<string, string | number | boolean>>,
): Record<string, string> {
  return Object.keys(preferences).length === 0
    ? {}
    : chunked('CAMOU_PREFS_', JSON.stringify(preferences));
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
 * What the browser is sent of the document is what the build reads: a key
 * the build declares `removed` (the canvas and glyph-spacing seeds,
 * `disableTheming` and others, which Camoufox 156 dropped) has no effect
 * and is left out, and so are the keys of {@link NEVER_APPLIED}. The
 * document itself keeps them as they were generated.
 *
 * @param pdfPath where the browser writes a printed page.
 * @throws {EngineError} `invalid_request` when the request's screen is not
 *   the fingerprint's: a persona's screen never changes.
 */
export function launchPlan(
  document: FingerprintDocument,
  request: OpenContextRequest,
  host: Pick<
    HostFacts,
    'installDir' | 'properties' | 'removedProperties' | 'locales' | 'fontconfig' | 'cpus'
  >,
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
  const config: CamoufoxConfig = Object.fromEntries(
    Object.entries({ ...generated.config, ...localeConfig(request) }).filter(
      ([key]) => !host.removedProperties.has(key) && !NEVER_APPLIED.includes(key),
    ),
  );
  validateConfig(config, host.properties);
  const side = (key: string, fallback: number) => {
    const value = config[key];
    return typeof value === 'number' ? Math.max(fallback, value) : fallback;
  };
  const preferences = {
    ...basePreferences(pdfPath),
    'webgl.enable-webgl2': generated.webgl2,
    'intl.locale.requested': firefoxUiLocale(request.locale, host.locales),
    ...webgpuPreferences(document.claimed_os, config),
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
    configEnv: { ...configEnvironment(config), ...preferenceEnvironment(preferences) },
    fontconfig: personaFontconfig(
      document.claimed_os,
      host.fontconfig[document.claimed_os],
      host.installDir,
    ),
    preferences,
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
