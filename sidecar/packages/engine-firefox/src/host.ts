import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import path from 'node:path';

import type { ClaimedOs } from '@kage/contract';
import { EngineError, allowedCpus } from '@kage/engine-core';

import { readArchiveEntry } from './omni.js';

/** Where the image unpacks the Camoufox build. */
export const DEFAULT_CAMOUFOX_DIR = '/opt/camoufox';

/**
 * The Camoufox build this engine is written and tested against
 * (`docs/versions.md`, fingerprint-critical). The engine refuses to start
 * on any other build: a different one generates and applies fingerprints
 * differently, and nothing here was measured on it.
 */
export const CAMOUFOX_VERSION = '156.0.1';
export const CAMOUFOX_RELEASE = 'beta.39';

/**
 * SHA-256 of the pinned build's `omni.ja`, the archive of the browser's own
 * scripts, its driver (Juggler) among them: the only archive this engine
 * runs (`docs/versions.md`).
 *
 * The build is fetched by the checksum of its release, so this is the
 * archive as upstream ships it. It is checked again when the engine starts
 * because what a page can see of the driver lives in it: the pinned build
 * runs no evaluation of a page as user input (spec §10.2, D33) and does not
 * register the driver where a page can probe for it, and an archive changed
 * in place would bring either back without a word.
 */
export const CAMOUFOX_OMNI_SHA256 =
  '85848ad445652be44f7b11186b14b7d52c381c269419738c0a2ec2dcff7675bc';

/** The archive of the build's own scripts, the driver's among them. */
export const CAMOUFOX_OMNI = 'omni.ja';

/** The browser binary inside the build directory. */
export const CAMOUFOX_BINARY = 'camoufox-bin';

/** The file in `omni.ja` that lists the user interface languages the build ships. */
const LOCALE_LIST = 'res/multilocale.txt';

/** Type names the build's `properties.json` uses for its configuration keys. */
export type PropertyType = 'str' | 'int' | 'uint' | 'double' | 'bool' | 'array' | 'dict';

/** What the engine needs to know about the machine and the build it runs. */
export interface HostFacts {
  /** The directory the build is unpacked in. */
  readonly installDir: string;
  /** The browser binary. */
  readonly binary: string;
  /** Firefox version of the build, for example `156.0.1`. */
  readonly version: string;
  /** Camoufox release of the build, for example `beta.39`. */
  readonly release: string;
  /** SHA-256 of the build's `omni.ja`: {@link CAMOUFOX_OMNI_SHA256}, or the engine would not run. */
  readonly omniSha256: string;
  /** Every configuration key the build declares, with its type, those it no longer reads included. */
  readonly properties: ReadonlyMap<string, PropertyType>;
  /**
   * The keys the build declares but no longer reads (`"removed"` in its
   * `properties.json`): they stay declared so that older documents validate,
   * and the engine never sends them.
   */
  readonly removedProperties: ReadonlySet<string>;
  /** The user interface languages the build ships (Firefox locale codes such as `de`, `en-GB`). */
  readonly locales: readonly string[];
  /** The build's own fontconfig file for each system it can claim. */
  readonly fontconfig: Readonly<Record<ClaimedOs, string>>;
  /** Ids of the CPUs this process may run on, ascending. */
  readonly cpus: readonly number[];
}

/** The name of the build's font and fontconfig directory for a claimed system. */
export const OS_DIRECTORY: Readonly<Record<ClaimedOs, string>> = {
  windows: 'windows',
  macos: 'macos',
  linux: 'linux',
};

async function readText(file: string, what: string): Promise<string> {
  try {
    return await readFile(file, 'utf8');
  } catch (error) {
    throw new EngineError(
      'browser_launch_failed',
      `no Camoufox build here: ${what} cannot be read at ${file}. Install the pinned build (\`just setup-camoufox\`) or set SIDECAR_CAMOUFOX_DIR.`,
      { cause: error },
    );
  }
}

interface PropertyEntry {
  readonly property?: unknown;
  readonly type?: unknown;
  readonly removed?: unknown;
}

function propertyEntries(text: string): PropertyEntry[] {
  const entries = JSON.parse(text) as unknown;
  if (!Array.isArray(entries)) {
    throw new TypeError('properties.json is not a list');
  }
  return entries as PropertyEntry[];
}

/** Reads the configuration keys a build declares from its `properties.json`. */
export function parseProperties(text: string): Map<string, PropertyType> {
  const properties = new Map<string, PropertyType>();
  for (const entry of propertyEntries(text)) {
    if (typeof entry.property === 'string' && typeof entry.type === 'string') {
      properties.set(entry.property, entry.type as PropertyType);
    }
  }
  return properties;
}

/**
 * Reads the keys a build declares as no longer read: each carries the
 * first release without it (`"removed": "156.0.1-beta.32"`).
 */
export function parseRemovedProperties(text: string): Set<string> {
  const removed = new Set<string>();
  for (const entry of propertyEntries(text)) {
    if (typeof entry.property === 'string' && entry.removed !== undefined) {
      removed.add(entry.property);
    }
  }
  return removed;
}

/** Reads the list of user interface languages a build ships, one comma-separated line. */
export function parseLocales(text: string): string[] {
  return text
    .split(/[,\s]+/)
    .map((locale) => locale.trim())
    .filter((locale) => locale !== '');
}

/**
 * Collects the facts about the installed Camoufox build.
 *
 * The version is read from the build's own `version.json`, written when the
 * pinned archive was unpacked; the launcher's download is never used,
 * because it does not install a pinned build (`docs/versions.md`).
 *
 * @throws {EngineError} `browser_launch_failed` when there is no build, it
 *   is not the pinned one, or its `omni.ja` is not the pinned archive.
 */
export async function detectHost(installDir: string = DEFAULT_CAMOUFOX_DIR): Promise<HostFacts> {
  const versionText = await readText(path.join(installDir, 'version.json'), 'its version');
  let installed: { version?: unknown; release?: unknown };
  try {
    installed = JSON.parse(versionText) as { version?: unknown; release?: unknown };
  } catch {
    installed = {};
  }
  if (installed.version !== CAMOUFOX_VERSION || installed.release !== CAMOUFOX_RELEASE) {
    throw new EngineError(
      'browser_launch_failed',
      `the Camoufox build at ${installDir} is ${String(installed.version)}-${String(installed.release)}; this engine runs ${CAMOUFOX_VERSION}-${CAMOUFOX_RELEASE} only`,
    );
  }
  // The archive is the pinned one or the engine does not start: a changed
  // driver would bring back what a page must not see, without a word.
  const omniFile = path.join(installDir, CAMOUFOX_OMNI);
  const omni = await readFile(omniFile).catch((error: unknown) => {
    throw new EngineError(
      'browser_launch_failed',
      `no Camoufox build here: its ${CAMOUFOX_OMNI} cannot be read at ${omniFile}. Install the pinned build (\`just setup-camoufox\`).`,
      { cause: error },
    );
  });
  const omniSha256 = createHash('sha256').update(omni).digest('hex');
  if (omniSha256 !== CAMOUFOX_OMNI_SHA256) {
    throw new EngineError(
      'browser_launch_failed',
      `the Camoufox build at ${installDir} is not the pinned build's: its ${CAMOUFOX_OMNI} is ${omniSha256}, this engine runs ${CAMOUFOX_OMNI_SHA256} only. \`just setup-camoufox\` installs the pinned build.`,
    );
  }
  const localeList = readArchiveEntry(omni, LOCALE_LIST);
  const propertiesText = await readText(
    path.join(installDir, 'properties.json'),
    'its configuration keys',
  );
  const conf = (os: ClaimedOs) =>
    readText(
      path.join(installDir, 'fontconfig', OS_DIRECTORY[os], 'fonts.conf'),
      `its fontconfig file for ${os}`,
    );
  return {
    installDir,
    binary: path.join(installDir, CAMOUFOX_BINARY),
    version: CAMOUFOX_VERSION,
    release: CAMOUFOX_RELEASE,
    omniSha256,
    properties: parseProperties(propertiesText),
    removedProperties: parseRemovedProperties(propertiesText),
    // A build without the list ships its default language only.
    locales: localeList === null ? ['en-US'] : parseLocales(localeList.toString('utf8')),
    fontconfig: {
      windows: await conf('windows'),
      macos: await conf('macos'),
      linux: await conf('linux'),
    },
    cpus: allowedCpus(),
  };
}
