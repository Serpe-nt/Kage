import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { readFile } from 'node:fs/promises';
import { pipeline } from 'node:stream/promises';
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

/**
 * SHA-256 of the build's `omni.ja` with the Juggler backport, the only
 * archive this engine runs (ADR 0010, `docs/versions.md`).
 *
 * The driver inside the pinned build runs every evaluation of a page as
 * user input, so a page nobody touched reads `navigator.userActivation` as
 * active (spec §10.2). `scripts/patch-camoufox-juggler.mjs` takes the four
 * statements out, as upstream did in later builds, and turns the release's
 * archive into exactly this one; the image build and
 * `scripts/setup-camoufox.sh` run it. The same value is
 * `CAMOUFOX_OMNI_PATCHED_SHA256` in `deploy/docker/sidecar.Dockerfile`.
 */
export const CAMOUFOX_OMNI_SHA256 =
  'ad3799d8c96fa80ba26a58cb1aaa28f525801d9ecc727a3e2be9262318ccbf5c';

/** The archive of the build's own scripts, the driver's among them. */
export const CAMOUFOX_OMNI = 'omni.ja';

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
  /** SHA-256 of the build's `omni.ja`: {@link CAMOUFOX_OMNI_SHA256}, or the engine would not run. */
  readonly omniSha256: string;
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

/** SHA-256 of a file, read as a stream: the archive has 43 MB. */
async function sha256Of(file: string): Promise<string> {
  const hash = createHash('sha256');
  await pipeline(createReadStream(file), hash);
  return hash.digest('hex');
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
 * @throws {EngineError} `browser_launch_failed` when there is no build, it
 *   is not the pinned one, or it does not hold the Juggler backport.
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
  // The archive is the patched one or the engine does not start: an
  // unpatched build would bring the tell back without a word.
  const omniFile = path.join(installDir, CAMOUFOX_OMNI);
  const omniSha256 = await sha256Of(omniFile).catch((error: unknown) => {
    throw new EngineError(
      'browser_launch_failed',
      `no Camoufox build here: its ${CAMOUFOX_OMNI} cannot be read at ${omniFile}. Install the pinned build (\`just setup-camoufox\`).`,
      { cause: error },
    );
  });
  if (omniSha256 !== CAMOUFOX_OMNI_SHA256) {
    throw new EngineError(
      'browser_launch_failed',
      `the Camoufox build at ${installDir} does not hold the Juggler backport (ADR 0010): its ${CAMOUFOX_OMNI} is ${omniSha256}, this engine runs ${CAMOUFOX_OMNI_SHA256} only. \`just setup-camoufox\` applies it to the pinned build.`,
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
    omniSha256,
    properties,
    fontconfig: {
      windows: await conf('windows'),
      macos: await conf('macos'),
      linux: await conf('linux'),
    },
    cpus: allowedCpus(),
  };
}
