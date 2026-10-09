import { createHash } from 'node:crypto';
import { mkdir, rename, writeFile } from 'node:fs/promises';
import path from 'node:path';

import type { ArtifactRef } from '@kage/contract';

const SEGMENT = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;

/** A path that would leave the artifacts volume, or is not a plain relative path. */
export class ArtifactPathError extends Error {
  override readonly name = 'ArtifactPathError';
}

/**
 * Checks a path relative to the artifacts volume and returns its segments.
 *
 * Every segment starts with a letter or digit and holds only letters,
 * digits, dots, dashes and underscores, so `..`, hidden names, absolute
 * paths, back slashes and empty segments are all refused.
 *
 * @throws {ArtifactPathError} naming the path.
 */
export function artifactSegments(relative: string): string[] {
  const segments = relative.split('/');
  if (relative.length > 400 || segments.some((segment) => !SEGMENT.test(segment))) {
    throw new ArtifactPathError(
      `not a path below the artifacts volume: ${JSON.stringify(relative)}`,
    );
  }
  return segments;
}

/**
 * Writes files to the artifacts volume the sidecar shares with Kage (spec
 * §10.2) and describes them as the contract does.
 */
export class ArtifactWriter {
  /** @param root the directory the artifacts volume is mounted at. */
  constructor(private readonly root: string) {}

  /**
   * Writes one file and returns its reference.
   *
   * The file is written under a temporary name and renamed, so Kage never
   * reads half a file.
   *
   * @param relative path below the volume, with forward slashes.
   * @throws {ArtifactPathError} when the path would leave the volume.
   */
  async write(relative: string, data: Uint8Array, contentType: string): Promise<ArtifactRef> {
    const segments = artifactSegments(relative);
    const target = path.join(this.root, ...segments);
    await mkdir(path.dirname(target), { recursive: true, mode: 0o750 });
    const temporary = `${target}.part`;
    await writeFile(temporary, data, { mode: 0o640 });
    await rename(temporary, target);
    return {
      path: segments.join('/'),
      bytes: data.byteLength,
      sha256: createHash('sha256').update(data).digest('hex'),
      content_type: contentType,
    };
  }
}
