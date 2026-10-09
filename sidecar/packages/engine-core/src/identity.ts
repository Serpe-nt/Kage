import type { ClaimedOs } from '@kage/contract';

/**
 * How each claimed operating system shows in the places a page can read it
 * (spec §6.2): the `User-Agent`, `navigator.platform`, and the platform of
 * the client hints (`Sec-CH-UA-Platform`, `navigator.userAgentData`).
 */
interface OsMarks {
  readonly userAgent: RegExp;
  readonly platform: RegExp;
  /** The client-hint platform, exactly as browsers write it. */
  readonly hint: string;
}

const OS_MARKS: Readonly<Record<ClaimedOs, OsMarks>> = {
  windows: { userAgent: /\(Windows NT [0-9.]+;/, platform: /^Win(32|64)$/, hint: 'Windows' },
  macos: {
    userAgent: /\(Macintosh; Intel Mac OS X [0-9_.]+/,
    platform: /^MacIntel$/,
    hint: 'macOS',
  },
  linux: { userAgent: /\(X11; (Linux|Ubuntu|Fedora)/, platform: /^Linux /, hint: 'Linux' },
};

const OSES = Object.keys(OS_MARKS) as ClaimedOs[];

/** The operating system a `User-Agent` names; `null` when it is none of the three. */
export function osOfUserAgent(userAgent: string): ClaimedOs | null {
  return OSES.find((os) => OS_MARKS[os].userAgent.test(userAgent)) ?? null;
}

/** The operating system a `navigator.platform` value names. */
export function osOfPlatform(platform: string): ClaimedOs | null {
  return OSES.find((os) => OS_MARKS[os].platform.test(platform)) ?? null;
}

/**
 * The operating system a client-hint platform names. Accepts the header
 * form with its quotes (`"Windows"`) and the script form without.
 */
export function osOfHintPlatform(platform: string): ClaimedOs | null {
  const bare = platform.replace(/^"(.*)"$/, '$1');
  return OSES.find((os) => OS_MARKS[os].hint === bare) ?? null;
}

/** The operating system Firefox's `navigator.oscpu` names. */
export function osOfOscpu(oscpu: string): ClaimedOs | null {
  if (/^Windows NT /.test(oscpu)) {
    return 'windows';
  }
  if (/Mac OS X/.test(oscpu)) {
    return 'macos';
  }
  return /^Linux /.test(oscpu) ? 'linux' : null;
}

/** The operating system a `navigator.appVersion` names: the first word in its parentheses. */
export function osOfAppVersion(appVersion: string): ClaimedOs | null {
  const system = /\((Windows|Macintosh|X11)/.exec(appVersion)?.[1];
  return system === 'Windows'
    ? 'windows'
    : system === 'Macintosh'
      ? 'macos'
      : system === 'X11'
        ? 'linux'
        : null;
}

/**
 * The operating system a WebGL renderer string can only come from;
 * `null` for one that says nothing about the system (most do not).
 */
export function osOfWebglRenderer(renderer: string): ClaimedOs | null {
  if (/Direct3D/i.test(renderer)) {
    return 'windows';
  }
  if (/\bApple\b|OpenGL Engine|\bMetal\b/.test(renderer)) {
    return 'macos';
  }
  return /\bMesa\b|llvmpipe/i.test(renderer) ? 'linux' : null;
}

/**
 * Font families only one system ships. A page finds them by measuring
 * text; their presence says where the fonts come from, whatever the
 * `User-Agent` says.
 */
export const OS_FONTS: Readonly<Record<ClaimedOs, readonly string[]>> = {
  windows: [
    'Segoe UI',
    'Calibri',
    'Cambria',
    'Consolas',
    'Franklin Gothic Medium',
    'Gabriola',
    'Sylfaen',
    'MS Gothic',
  ],
  macos: [
    'Helvetica Neue',
    'Menlo',
    'Monaco',
    'Lucida Grande',
    'Geneva',
    'Avenir',
    'Optima',
    'PingFang SC',
    'Hiragino Sans',
    'Apple SD Gothic Neo',
  ],
  linux: [
    'DejaVu Sans',
    'DejaVu Sans Mono',
    'Liberation Sans',
    'Liberation Mono',
    'Cantarell',
    'Noto Sans',
    'Noto Mono',
    'Ubuntu',
    'Droid Sans Fallback',
    'Nimbus Sans',
    'FreeSans',
  ],
};

/** The client-hint platform of an operating system, as browsers write it. */
export function hintPlatformOf(os: ClaimedOs): string {
  return OS_MARKS[os].hint;
}

/** The browser major a `User-Agent` names (`Chrome/155` or `Firefox/152`). */
export function browserMajorOfUserAgent(userAgent: string): number | null {
  const match = /(?:Chrome|Firefox)\/(\d+)\./.exec(userAgent);
  return match?.[1] === undefined ? null : Number.parseInt(match[1], 10);
}

/** The major of a full version such as `155.0.8059.39`. */
export function majorOfVersion(version: string): number | null {
  const major = Number.parseInt(version.split('.')[0] ?? '', 10);
  return Number.isInteger(major) && major > 0 ? major : null;
}
