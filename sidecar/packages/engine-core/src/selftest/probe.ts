import type { ExpectedIdentity, Finding } from '@kage/contract';
import { z } from 'zod';

import {
  OS_FONTS,
  browserMajorOfUserAgent,
  majorOfVersion,
  osOfAppVersion,
  osOfHintPlatform,
  osOfOscpu,
  osOfPlatform,
  osOfUserAgent,
  osOfWebglRenderer,
} from '../identity.js';

import { FindingList } from './findings.js';

/**
 * Kage's coherence probe (spec §6.2).
 *
 * The probe page (`tests/fixtures/site/probe/`) only observes: it reads the
 * same values in the window, three kinds of iframes and two kinds of
 * workers, and publishes them as JSON. The judging happens here, against
 * what the persona claims, so the page holds no expectation that a site
 * could read.
 */

const text = z.string().nullable().catch(null);

const HighEntropySchema = z.object({
  platform: text,
  platform_version: text,
  architecture: text,
  bitness: text,
  ua_full_version: text,
});

const count = z.number().nullable().catch(null);

const ScopeSchema = z.object({
  user_agent: text,
  platform: text,
  /** Firefox only, and only in windows: workers have no such property. */
  oscpu: text,
  app_version: text,
  language: text,
  intl_locale: text,
  languages: z.array(z.string()).nullable().catch(null),
  hardware_concurrency: z.number().nullable().catch(null),
  device_memory: z.number().nullable().catch(null),
  timezone: text,
  uad_platform: text,
  high_entropy: HighEntropySchema.nullable().catch(null),
  webgl: z
    .object({
      vendor: text,
      renderer: text,
      plain_renderer: text,
      version: text,
      shading_language: text,
      max_texture_size: count,
      extensions: count,
    })
    .nullable()
    .catch(null),
  /** The marker font families a line of text measured differently in. */
  fonts: z.array(z.string()).nullable().catch(null),
  /** Windows and frames only. */
  screen: z
    .object({
      width: count,
      height: count,
      avail_width: count,
      avail_height: count,
      color_depth: count,
    })
    .nullable()
    .catch(null),
  error: text,
});
export type ProbeScope = z.infer<typeof ScopeSchema>;

const RequestHeadersSchema = z
  .array(z.tuple([z.string(), z.string()]))
  .nullable()
  .catch(null);

/** What only the top window can tell: the desktop around the browser. */
const EnvironmentSchema = z.object({
  window: z
    .object({
      inner_width: count,
      inner_height: count,
      outer_width: count,
      outer_height: count,
      avail_left: count,
      avail_top: count,
      device_pixel_ratio: count,
    })
    .nullable()
    .catch(null),
  media_devices: z
    .object({ audioinput: z.number(), audiooutput: z.number(), videoinput: z.number() })
    .nullable()
    .catch(null),
  audio: z.object({ sample_rate: count, max_channel_count: count }).nullable().catch(null),
  voices: count,
});

const flag = z.boolean().nullable().catch(null);

/**
 * What the page says of user activation, against the input it received.
 * Sampled when its script starts and again half a second after `load`.
 */
const ActivationSchema = z.object({
  samples: z.array(
    z.object({
      at: z.string(),
      ms: count,
      /** `navigator.userActivation`; `null` in a browser without it. */
      is_active: flag,
      has_been_active: flag,
      /** State of an `AudioContext` made for the sample; `null` without one. */
      audio_state: text,
    }),
  ),
  /** Whether the page before this one was of the same origin, by the referrer. */
  reached_from_own_origin: z.boolean(),
  /**
   * `navigator.getAutoplayPolicy()` for media elements and for an
   * `AudioContext`, where the browser has the call (Firefox); else `null`.
   */
  autoplay_policy: z
    .object({ mediaelement: text, audiocontext: text })
    .nullable()
    .catch(null)
    .optional(),
  /** Trusted input events of any kind the page received. */
  trusted_input_events: z.number(),
  /** Of them, the ones a browser grants activation for (a key or a button going down). */
  activating_input_events: z.number(),
  input_types: z.array(z.string()),
});
export type ProbeActivation = z.infer<typeof ActivationSchema>;

/** What the probe page publishes. */
export const ProbeReportSchema = z.object({
  kage_probe: z.literal(1),
  /** Absent from a probe page of before M5. */
  environment: EnvironmentSchema.nullable().catch(null).optional(),
  /** Absent from a probe page of before M5.1. */
  activation: ActivationSchema.nullable().catch(null).optional(),
  scopes: z.object({
    window: ScopeSchema,
    iframe_same_origin: ScopeSchema.nullable().catch(null),
    iframe_srcdoc: ScopeSchema.nullable().catch(null),
    iframe_blank: ScopeSchema.nullable().catch(null),
    worker: ScopeSchema.nullable().catch(null),
    blob_worker: ScopeSchema.nullable().catch(null),
  }),
  automation: z.object({
    webdriver: z.boolean().nullable().catch(null),
    /** How often the browser read the `stack` of an error that was only logged. */
    console_stack_reads: z.number(),
    getters_native: z.boolean(),
    illegal_invocation_throws: z.boolean(),
    navigator_own_properties: z.array(z.string()),
  }),
  /**
   * Request headers as the server saw them, in order; `null` where the host
   * of the probe cannot tell (a static host tells nothing).
   */
  requests: z.object({
    navigation: RequestHeadersSchema,
    iframe: RequestHeadersSchema,
    fetch: RequestHeadersSchema,
    worker_script: RequestHeadersSchema,
    worker_fetch: RequestHeadersSchema,
  }),
});
export type ProbeReport = z.infer<typeof ProbeReportSchema>;

/** The scopes of a report, in a fixed order. */
export const PROBE_SCOPES = [
  'window',
  'iframe_same_origin',
  'iframe_srcdoc',
  'iframe_blank',
  'worker',
  'blob_worker',
] as const;
export type ProbeScopeName = (typeof PROBE_SCOPES)[number];

/** Reads the JSON the probe page published; `null` when it is not a probe report. */
export function parseProbeReport(json: string): ProbeReport | null {
  try {
    const parsed = ProbeReportSchema.safeParse(JSON.parse(json));
    return parsed.success ? parsed.data : null;
  } catch {
    return null;
  }
}

const header = (headers: readonly (readonly [string, string])[], name: string): string | null =>
  headers.find(([key]) => key.toLowerCase() === name)?.[1] ?? null;

function agreement(
  list: FindingList,
  check: string,
  scopes: readonly (readonly [ProbeScopeName, ProbeScope])[],
  read: (scope: ProbeScope) => string | null,
): void {
  const values = scopes
    .map(([name, scope]) => [name, read(scope)] as const)
    .filter((entry): entry is readonly [ProbeScopeName, string] => entry[1] !== null);
  const distinct = new Set(values.map(([, value]) => value));
  if (values.length < 2) {
    return;
  }
  list.check(
    check,
    distinct.size === 1,
    true,
    distinct.size === 1
      ? (values[0]?.[1] ?? null)
      : values.map(([name, value]) => `${name}=${value}`).join(' | '),
    'the same value in every scope',
  );
}

/**
 * Judges what the page saw of user activation (spec §10.2).
 *
 * A browser grants activation for input, and a browser driver may grant it
 * for reading the page: both engines' drivers mark an evaluation as a user
 * gesture. A page that is active, or has been, without one input event
 * that grants activation was therefore touched by something that is no
 * person, which a detector can see by the same comparison. That is an
 * automation flag and a hard failure.
 *
 * One case is the browser's own: Chrome carries a person's activation into
 * a page reached by a link of the same origin. The self-test loads the
 * probe by a typed navigation, where nothing is carried; a probe reached
 * from its own origin is not judged.
 *
 * Sound that may start by itself is reported next to it, as a warning: an
 * `AudioContext` that runs, or a browser that says `allowed` when asked
 * (`navigator.getAutoplayPolicy()`, Firefox), on a page nobody touched. On
 * Chrome it follows activation. A browser nobody configured blocks it
 * (measured: a stock Firefox says `allowed-muted` and `disallowed`, the
 * Camoufox build as it ships says `allowed`, which is why the engine sets
 * Firefox's default). It is no failure: a user may have chosen it.
 */
function judgeActivation(list: FindingList, activation: ProbeActivation | null): void {
  if (activation === null || activation.samples.length === 0) {
    // A probe page of before M5.1, on a host nobody updated: the check
    // that matters most for a driven browser did not run.
    list.add('automation.user_activation', 'warn', {
      observed: 'the probe page reported no user activation: it is older than this check',
      expected: 'the probe page of this version',
    });
    return;
  }
  const { samples } = activation;
  const explained = activation.activating_input_events > 0 || activation.reached_from_own_origin;
  const input = `trusted input events: ${activation.trusted_input_events}, of them granting activation: ${activation.activating_input_events}${activation.reached_from_own_origin ? ', reached from its own origin' : ''}`;
  const told = samples
    .map(
      (sample) =>
        `${sample.at}: is_active=${String(sample.is_active)} has_been_active=${String(sample.has_been_active)}`,
    )
    .join(' | ');
  if (samples.some((sample) => sample.is_active === null || sample.has_been_active === null)) {
    list.add('automation.user_activation', 'warn', {
      observed: `${told} | ${input}`,
      expected: 'a browser with navigator.userActivation',
    });
  } else {
    const active = samples.some(
      (sample) => sample.is_active === true || sample.has_been_active === true,
    );
    list.check(
      'automation.user_activation',
      !active || explained,
      true,
      `${told} | ${input}`,
      'no user activation before an input event that grants it',
    );
  }
  const audio = samples.filter((sample) => sample.audio_state !== null);
  const policy = activation.autoplay_policy ?? null;
  if (audio.length > 0 || policy !== null) {
    const allowed =
      audio.some((sample) => sample.audio_state === 'running') ||
      policy?.mediaelement === 'allowed' ||
      policy?.audiocontext === 'allowed';
    const heard = [
      ...audio.map((sample) => `${sample.at}: ${sample.audio_state ?? ''}`),
      ...(policy === null
        ? []
        : [
            `autoplay policy: mediaelement=${policy.mediaelement ?? 'null'} audiocontext=${policy.audiocontext ?? 'null'}`,
          ]),
      input,
    ];
    list.add('automation.audio_autoplay', allowed && !explained ? 'warn' : 'pass', {
      observed: heard.join(' | '),
      expected: 'sound that waits for the user',
    });
  }
}

/**
 * Judges a probe report against the identity a persona claims.
 *
 * Hard failures are the ones spec §6.2 names: an automation flag, a scope
 * whose operating system, browser major, time zone or languages are not the
 * claimed ones, scopes that disagree with each other, and request headers
 * that disagree with the window. A scope that did not report is a warning:
 * a page may forbid workers, and that is not the persona's fault.
 */
export function judgeProbe(
  report: ProbeReport,
  expected: ExpectedIdentity,
  page: string,
  weight: number,
): Finding[] {
  const list = new FindingList(page, weight);
  const { automation, requests } = report;

  list.check(
    'automation.webdriver',
    automation.webdriver === false,
    true,
    String(automation.webdriver),
    'false',
  );
  list.check(
    'automation.console_getter',
    automation.console_stack_reads === 0,
    true,
    String(automation.console_stack_reads),
    '0',
  );
  const native =
    automation.getters_native &&
    automation.illegal_invocation_throws &&
    automation.navigator_own_properties.length === 0;
  list.check(
    'automation.native_getters',
    native,
    true,
    `getters_native=${automation.getters_native} illegal_invocation_throws=${automation.illegal_invocation_throws} own=[${automation.navigator_own_properties.join(',')}]`,
    'native getters, no own properties on navigator',
  );

  judgeActivation(list, report.activation ?? null);

  const reporting: (readonly [ProbeScopeName, ProbeScope])[] = [];
  for (const name of PROBE_SCOPES) {
    const scope = report.scopes[name];
    if (scope?.user_agent == null || scope.error !== null) {
      list.add(`scope.${name}`, 'warn', { observed: scope?.error ?? 'did not report' });
      continue;
    }
    reporting.push([name, scope]);
    const userAgent = scope.user_agent;
    const marks = [
      osOfUserAgent(userAgent),
      scope.platform === null ? null : osOfPlatform(scope.platform),
      scope.oscpu == null ? expected.claimed_os : osOfOscpu(scope.oscpu),
      scope.app_version == null ? expected.claimed_os : osOfAppVersion(scope.app_version),
      scope.uad_platform === null ? expected.claimed_os : osOfHintPlatform(scope.uad_platform),
      scope.high_entropy?.platform == null
        ? expected.claimed_os
        : osOfHintPlatform(scope.high_entropy.platform),
    ];
    list.check(
      `os.${name}`,
      marks.every((os) => os === expected.claimed_os),
      true,
      `user_agent=${userAgent} | platform=${scope.platform ?? 'null'} | oscpu=${scope.oscpu ?? 'null'} | app_version=${scope.app_version ?? 'null'} | hint=${scope.uad_platform ?? 'null'} | high_entropy=${scope.high_entropy?.platform ?? 'null'}`,
      expected.claimed_os,
    );
    const majors = [
      browserMajorOfUserAgent(userAgent),
      scope.high_entropy?.ua_full_version == null
        ? expected.browser_major
        : majorOfVersion(scope.high_entropy.ua_full_version),
    ];
    list.check(
      `browser_major.${name}`,
      majors.every((major) => major === expected.browser_major),
      true,
      majors.join(' | '),
      String(expected.browser_major),
    );
    list.equal(`timezone.${name}`, scope.timezone, expected.timezone, true);
    list.equal(
      `languages.${name}`,
      scope.languages?.join(',') ?? null,
      expected.languages.join(','),
      true,
    );
  }

  agreement(list, 'coherence.user_agent', reporting, (scope) => scope.user_agent);
  agreement(list, 'coherence.platform', reporting, (scope) => scope.platform);
  agreement(list, 'coherence.hint_platform', reporting, (scope) => scope.uad_platform);
  agreement(list, 'coherence.hardware_concurrency', reporting, (scope) =>
    scope.hardware_concurrency === null ? null : String(scope.hardware_concurrency),
  );
  agreement(list, 'coherence.device_memory', reporting, (scope) =>
    scope.device_memory === null ? null : String(scope.device_memory),
  );
  agreement(list, 'coherence.app_version', reporting, (scope) => scope.app_version ?? null);
  agreement(list, 'coherence.language', reporting, (scope) => scope.language ?? null);
  agreement(list, 'coherence.intl_locale', reporting, (scope) => scope.intl_locale ?? null);
  agreement(list, 'coherence.webgl', reporting, (scope) =>
    scope.webgl === null
      ? null
      : [
          scope.webgl.vendor,
          scope.webgl.renderer,
          scope.webgl.plain_renderer,
          scope.webgl.version,
          scope.webgl.shading_language,
          scope.webgl.max_texture_size,
          scope.webgl.extensions,
        ]
          .map((value) => String(value ?? ''))
          .join(' / '),
  );
  // Fonts and the screen are read by measuring and by asking: a worker's
  // canvas must find the same families as the window's.
  agreement(list, 'coherence.fonts', reporting, (scope) => scope.fonts?.join(',') ?? null);
  agreement(list, 'coherence.screen', reporting, (scope) =>
    scope.screen == null
      ? null
      : [
          scope.screen.width,
          scope.screen.height,
          scope.screen.avail_width,
          scope.screen.avail_height,
          scope.screen.color_depth,
        ].join('x'),
  );

  const window = report.scopes.window;
  list.add('webgl.available', window.webgl?.renderer == null ? 'warn' : 'pass', {
    observed: window.webgl?.renderer ?? 'no WebGL',
  });
  const renderer = window.webgl?.renderer ?? null;
  if (renderer !== null) {
    const named = osOfWebglRenderer(renderer);
    list.check(
      'webgl.os',
      named === null || named === expected.claimed_os,
      true,
      renderer,
      `a renderer of ${expected.claimed_os}, or one that names no system`,
    );
  }
  if (window.fonts != null) {
    const found = new Set(window.fonts);
    const own = OS_FONTS[expected.claimed_os].filter((family) => found.has(family));
    const foreign = (Object.keys(OS_FONTS) as (keyof typeof OS_FONTS)[])
      .filter((os) => os !== expected.claimed_os)
      .flatMap((os) => OS_FONTS[os].filter((family) => found.has(family)));
    list.check(
      'fonts.foreign',
      foreign.length === 0,
      true,
      foreign.length === 0 ? 'none' : foreign.join(', '),
      `no font only another system than ${expected.claimed_os} ships`,
    );
    // A Linux machine may have next to no fonts; Windows and macOS always
    // ship theirs.
    const enough = own.length >= (expected.claimed_os === 'linux' ? 1 : 3);
    if (enough || expected.claimed_os !== 'linux') {
      list.check(
        'fonts.own',
        enough,
        true,
        own.length === 0 ? 'none' : own.join(', '),
        `fonts ${expected.claimed_os} ships`,
      );
    } else {
      list.add('fonts.own', 'warn', { observed: 'no font a Linux desktop ships' });
    }
  }
  // What marks a browser without a desktop around it. None of these is an
  // identity mismatch, so none is hard; each is a known tell worth seeing.
  const environment = report.environment ?? null;
  if (environment !== null) {
    const screen = window.screen ?? null;
    if (screen !== null) {
      const full = screen.avail_width === screen.width && screen.avail_height === screen.height;
      list.add('environment.work_area', full ? 'warn' : 'pass', {
        observed: `${screen.avail_width ?? 0}x${screen.avail_height ?? 0} of ${screen.width ?? 0}x${screen.height ?? 0}`,
        expected: 'a work area smaller than the screen, as a desktop with a panel has',
      });
    }
    const devices = environment.media_devices;
    const total =
      devices === null ? 0 : devices.audioinput + devices.audiooutput + devices.videoinput;
    list.add('environment.media_devices', total === 0 ? 'warn' : 'pass', {
      observed:
        devices === null
          ? 'not readable'
          : `audioinput=${devices.audioinput} audiooutput=${devices.audiooutput} videoinput=${devices.videoinput}`,
      expected: 'at least one device',
    });
    const audio = environment.audio;
    list.add(
      'environment.audio',
      audio !== null && (audio.max_channel_count ?? 0) > 0 ? 'pass' : 'warn',
      {
        observed:
          audio === null
            ? 'no AudioContext'
            : `sample_rate=${audio.sample_rate ?? 0} max_channel_count=${audio.max_channel_count ?? 0}`,
        expected: 'an audio output with channels',
      },
    );
  }

  for (const [name, headers] of Object.entries(requests)) {
    if (headers === null) {
      continue;
    }
    list.equal(
      `headers.${name}.user_agent`,
      header(headers, 'user-agent'),
      window.user_agent ?? '',
      true,
    );
    const hint = header(headers, 'sec-ch-ua-platform');
    if (hint !== null) {
      list.check(
        `headers.${name}.platform`,
        osOfHintPlatform(hint) === expected.claimed_os,
        true,
        hint,
        expected.claimed_os,
      );
    }
  }
  if (requests.navigation !== null) {
    // A browser driven through request interception with the cache switched
    // off sends these two on every request; a person's browser sends neither
    // on a plain navigation.
    const forced = ['pragma', 'cache-control'].filter(
      (name) => header(requests.navigation ?? [], name) !== null,
    );
    list.check(
      'headers.navigation.cache',
      forced.length === 0,
      true,
      forced.length === 0 ? 'none' : forced.join(','),
      'no pragma, no cache-control',
    );
  }
  return list.findings;
}
