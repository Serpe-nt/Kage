import { cp, mkdir, readFile, rename, rm, stat, writeFile } from 'node:fs/promises';
import path from 'node:path';

import { majorOfVersion } from '@kage/engine-core';

/**
 * Where a profile is kept before its first start under a newer Firefox,
 * below the personas volume: `.upgrade/<persona id>/`. kage removes a copy
 * after `personas.upgrade_backup_days` (the Camoufox 156 milestone).
 */
export const UPGRADE_DIR = '.upgrade';

/** What a copy leaves out: caches Firefox builds again, and the locks of a running browser. */
const NOT_KEPT: ReadonlySet<string> = new Set(['cache2', 'startupCache', 'lock', '.parentlock']);

/** The Firefox major a profile last started under, from its `compatibility.ini`; `null` for a new profile. */
export async function profileMajor(userData: string): Promise<number | null> {
  const text = await readFile(path.join(userData, 'compatibility.ini'), 'utf8').catch(() => null);
  const version = text === null ? undefined : /^LastVersion=(\d+)\./m.exec(text)?.[1];
  return version === undefined ? null : Number(version);
}

/** What `upgrade.json` next to a kept profile says. */
export interface KeptProfile {
  readonly persona_id: string;
  /** The profile directory below the personas volume, as the open request named it. */
  readonly profile_dir: string;
  /** The Firefox major the profile last started under. */
  readonly from_major: number;
  /** The browser it is about to start under. */
  readonly to_version: string;
  readonly copied_at: string;
}

/** Where a persona's kept profile is: `<personas>/.upgrade/<persona id>`. */
export function keptProfileDir(personasDir: string, personaId: string): string {
  return path.join(personasDir, UPGRADE_DIR, personaId);
}

/**
 * Keeps a copy of a persona's profile directory before its first start
 * under a newer Firefox major.
 *
 * Firefox moves a profile forward when a newer version opens it and
 * refuses it to an older one afterwards (measured: a 152 profile opened by
 * 156 gives no context under 152 within a minute), so going back to the
 * previous image needs the profile as it was. The copy is taken once, at
 * the first open under the new major, and later opens find it and take no
 * other. It is written next to its final place and moved there when it is
 * whole.
 *
 * @returns where the copy is, or `null` when none was taken: a new profile,
 *   one of this major already, or one kept before.
 * @throws {Error} when the copy cannot be written; the caller must not start
 *   the browser then, or the way back is gone.
 */
export async function keepBeforeUpgrade(options: {
  readonly personasDir: string;
  readonly personaId: string;
  readonly profileDir: string;
  readonly relativeProfileDir: string;
  readonly userData: string;
  readonly hostVersion: string;
  readonly now: Date;
}): Promise<string | null> {
  const from = await profileMajor(options.userData);
  const to = majorOfVersion(options.hostVersion);
  if (from === null || to === null || from >= to) {
    return null;
  }
  const kept = keptProfileDir(options.personasDir, options.personaId);
  if (
    await stat(kept).then(
      () => true,
      () => false,
    )
  ) {
    return null;
  }
  const partial = `${kept}.partial`;
  await rm(partial, { recursive: true, force: true });
  await mkdir(path.dirname(kept), { recursive: true, mode: 0o700 });
  try {
    await cp(options.profileDir, partial, {
      recursive: true,
      verbatimSymlinks: true,
      preserveTimestamps: true,
      filter: (source) => !NOT_KEPT.has(path.basename(source)),
    });
    const note: KeptProfile = {
      persona_id: options.personaId,
      profile_dir: options.relativeProfileDir,
      from_major: from,
      to_version: options.hostVersion,
      copied_at: options.now.toISOString(),
    };
    await writeFile(path.join(partial, 'upgrade.json'), `${JSON.stringify(note)}\n`, {
      mode: 0o600,
    });
    await rename(partial, kept);
  } catch (error) {
    await rm(partial, { recursive: true, force: true });
    throw error;
  }
  return kept;
}
