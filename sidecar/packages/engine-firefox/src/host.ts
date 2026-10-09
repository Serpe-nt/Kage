import { readFile } from 'node:fs/promises';
import path from 'node:path';

import type { ClaimedOs } from '@kage/contract';
import { EngineError, allowedCpus } from '@kage/engine-core';

/** Where the image unpacks the Camoufox build. */
export const DEFAULT_CAMOUFOX_DIR = '/opt/camoufox';

/**
 * The Camoufox build this engine is written and tested against
 * (`docs/versions.md`, fingerprint-critical). The engine refuses to start
 * on any other build: a different one generates and applies fingerprints
 * differently, and nothing here was measured on it.
 */
export const CAMOUFOX_VERSION = '152.0.4';
export const CAMOUFOX_RELEASE = 'beta.30';

/** The browser binary inside the build directory. */
export const CAMOUFOX_BINARY = 'camoufox-bin';

/** Type names the build's `properties.json` uses for its configuration keys. */
export type PropertyType = 'str' | 'int' | 'uint' | 'double' | 'bool' | 'array' | 'dict';

/** What the engine needs to know about the machine and the build it runs. */
export interface HostFacts {
  /** The directory the build is unpacked in. */
  readonly installDir: string;
  /** The browser binary. */
  readonly binary: string;
  /** Firefox version of the build, for example `152.0.4`. */
  readonly version: string;
  /** Camoufox release of the build, for example `beta.30`. */
  readonly release: string;
  /** Every configuration key the build knows, with its type. */
  readonly properties: ReadonlyMap<string, PropertyType>;
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

/** Reads the configuration keys a build knows from its `properties.json`. */
export function parseProperties(text: string): Map<string, PropertyType> {
  const properties = new Map<string, PropertyType>();
  const entries = JSON.parse(text) as unknown;
  if (!Array.isArray(entries)) {
    throw new TypeError('properties.json is not a list');
  }
  for (const entry of entries as { property?: unknown; type?: unknown }[]) {
    if (typeof entry.property === 'string' && typeof entry.type === 'string') {
      properties.set(entry.property, entry.type as PropertyType);
    }
  }
  return properties;
}

/**
 * Collects the facts about the installed Camoufox build.
 *
 * The version is read from the build's own `version.json`, written when the
 * pinned archive was unpacked; the launcher's download is never used,
 * because it does not install a pinned build (`docs/versions.md`).
 *
 * @throws {EngineError} `browser_launch_failed` when there is no build, or
 *   it is not the pinned one.
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
  const properties = parseProperties(
    await readText(path.join(installDir, 'properties.json'), 'its configuration keys'),
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
    properties,
    fontconfig: { windows: await conf('windows'), macos: await conf('macos'), linux: await conf('linux') },
    cpus: allowedCpus(),
  };
}
