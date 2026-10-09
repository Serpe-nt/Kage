import type { GlMode, InjectMode, OpenContextRequest } from '@kage/contract';
import {
  EngineError,
  formatCpuList,
  keyboardLayoutOf,
  personaCpus,
  type FingerprintDocument,
  type WorkArea,
} from '@kage/engine-core';

export { personaCpus } from '@kage/engine-core';

/**
 * Everything a persona context is launched with, as plain data.
 *
 * The plan is a pure function of the persisted fingerprint, the open
 * request and the host's CPUs (spec §6.1 step 3: the same document is
 * applied the same way on every open). It holds launch parameters only:
 * with `inject: none` nothing else makes the persona, so what a page reads
 * is true in the window, in every frame and in every worker (ADR 0009).
 */
export interface LaunchPlan {
  /** The persona's own X display: its screen in physical pixels. */
  readonly display: { readonly width: number; readonly height: number; readonly depth: number };
  /** What the desktop's top bar leaves free of the screen; announced on the display. */
  readonly workArea: WorkArea;
  /** The keyboard layout of the display: the one people of the persona's locale type on. */
  readonly layout: string;
  /** The browser window: the whole work area, as a maximized window has it. */
  readonly window: {
    readonly x: number;
    readonly y: number;
    readonly width: number;
    readonly height: number;
  };
  /** Environment of the browser process, besides the display. */
  readonly env: Readonly<Record<string, string>>;
  /** Chrome arguments on top of the driver's own. */
  readonly args: readonly string[];
  /** CPUs the browser may run on; their count is `navigator.hardwareConcurrency`. */
  readonly cpus: readonly number[];
  /** Keys written into the profile's `Preferences` before launch. */
  readonly preferences: ProfilePreferences;
  /** The proxy with a bypass list that sends loopback through it too. */
  readonly proxy: {
    readonly server: string;
    readonly username: string;
    readonly password: string;
    readonly bypass: string;
  };
  /** The point the geolocation API reports once a site has permission. */
  readonly geolocation: {
    readonly latitude: number;
    readonly longitude: number;
    readonly accuracy: number;
  } | null;
  readonly gl: GlMode;
  readonly inject: InjectMode;
}

/** The keys of a Chrome profile's `Preferences` that Kage owns. */
export interface ProfilePreferences {
  readonly intl: { readonly accept_languages: string; readonly selected_languages: string };
  readonly webrtc: { readonly ip_handling_policy: string };
}

/**
 * Height in pixels of the bar a desktop keeps at the top of the screen
 * (GNOME's, on the distribution the image is built on). A browser on a
 * bare X server reports a work area as large as the screen, which no
 * desktop with a panel does; the display announces this bar and the
 * window is placed below it, so `screen.availHeight` and `screen.availTop`
 * are a desktop's.
 */
export const PANEL_HEIGHT = 32;

/**
 * Chromium's rule that sends loopback through the proxy as well. Without it
 * a page that asks for `localhost` would reach the sidecar's own network
 * instead of leaving through the proxy (spec §19).
 */
export const LOOPBACK_THROUGH_PROXY = '<-loopback>';

/**
 * WebRTC must not send UDP around the proxy: a STUN request from the host
 * tells a page the host's real address. With this policy nothing leaves
 * outside the proxy; the price is that no ICE candidate is gathered at all.
 */
export const WEBRTC_POLICY = 'disable_non_proxied_udp';

const GL_ARGS: Readonly<Record<GlMode, readonly string[]>> = {
  none: [],
  swiftshader: ['--enable-unsafe-swiftshader'],
  // Mesa's software renderer through ANGLE: a renderer string real Linux
  // machines without GPU acceleration report, the same in window and worker.
  llvmpipe: ['--use-gl=angle', '--use-angle=gl', '--ignore-gpu-blocklist'],
  // A GPU of the host mapped into the container (/dev/dri): Chrome's defaults.
  host: [],
};

/** What the plan needs to know about the host. */
export interface PlanHost {
  /** Ids of the CPUs the sidecar may run on. */
  readonly cpus: readonly number[];
  readonly gl: GlMode;
}

/**
 * Builds the launch plan of a context.
 *
 * @throws {EngineError} `invalid_request` when the request's screen is not
 *   the fingerprint's: a persona's screen never changes.
 */
export function launchPlan(
  document: FingerprintDocument,
  request: OpenContextRequest,
  host: PlanHost,
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
  const physical = {
    width: Math.round(screen.width * screen.dpr),
    height: Math.round(screen.height * screen.dpr),
  };
  const languages = request.languages.join(',');
  const locale = request.locale;
  const posixLocale = locale.replace('-', '_');
  const window = {
    x: 0,
    y: PANEL_HEIGHT,
    width: screen.width,
    height: screen.height - PANEL_HEIGHT,
  };
  const args = [
    `--window-position=${window.x},${window.y}`,
    `--window-size=${window.width},${window.height}`,
    `--lang=${locale}`,
    ...(screen.dpr === 1 ? [] : [`--force-device-scale-factor=${screen.dpr}`]),
    ...GL_ARGS[host.gl],
  ];
  return {
    // 24 bits is what an X display has; the generator's Linux fingerprints say the same.
    display: { ...physical, depth: 24 },
    workArea: {
      x: 0,
      y: Math.round(PANEL_HEIGHT * screen.dpr),
      width: physical.width,
      height: physical.height - Math.round(PANEL_HEIGHT * screen.dpr),
    },
    layout: keyboardLayoutOf(locale),
    window,
    env: {
      TZ: request.timezone,
      LANG: `${posixLocale}.UTF-8`,
      LANGUAGE: posixLocale,
    },
    args,
    cpus: personaCpus(document.seed, document.native.hardware_concurrency, host.cpus),
    preferences: {
      intl: { accept_languages: languages, selected_languages: languages },
      webrtc: { ip_handling_policy: WEBRTC_POLICY },
    },
    proxy: {
      server: request.proxy.server,
      username: request.proxy.username,
      password: request.proxy.password,
      bypass: LOOPBACK_THROUGH_PROXY,
    },
    geolocation:
      request.geolocation === null
        ? null
        : {
            latitude: request.geolocation.latitude,
            longitude: request.geolocation.longitude,
            accuracy: request.geolocation.accuracy_m,
          },
    gl: host.gl,
    inject: document.inject,
  };
}

/** The value of `KAGE_CPU_LIST` for a plan. */
export function cpuListOf(plan: LaunchPlan): string {
  return formatCpuList(plan.cpus);
}
