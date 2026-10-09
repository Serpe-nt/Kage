import { createHash } from 'node:crypto';

import type { GlMode, InjectMode, OpenContextRequest } from '@kage/contract';
import { EngineError, type FingerprintDocument } from '@kage/engine-core';

import { formatCpuList } from './host.js';

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
  /** The browser window: the whole screen, as a maximized window has it. */
  readonly window: { readonly width: number; readonly height: number };
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
 * The CPUs of a persona: as many as its fingerprint says, at most as many
 * as the host has, starting at a position derived from the seed so that the
 * personas of one host do not all sit on the first CPUs.
 */
export function personaCpus(seed: string, wanted: number, hostCpus: readonly number[]): number[] {
  const count = Math.max(1, Math.min(wanted, hostCpus.length));
  const start =
    createHash('sha256').update('kage-cpus-v1').update(seed).digest().readUInt32LE(0) %
    hostCpus.length;
  return Array.from(
    { length: count },
    (_unused, index) => hostCpus[(start + index) % hostCpus.length] ?? 0,
  ).sort((a, b) => a - b);
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
  const args = [
    '--window-position=0,0',
    `--window-size=${screen.width},${screen.height}`,
    `--lang=${locale}`,
    ...(screen.dpr === 1 ? [] : [`--force-device-scale-factor=${screen.dpr}`]),
    ...GL_ARGS[host.gl],
  ];
  return {
    // 24 bits is what an X display has; the generator's Linux fingerprints say the same.
    display: { ...physical, depth: 24 },
    window: { width: screen.width, height: screen.height },
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
