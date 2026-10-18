import { inflateRawSync } from 'node:zlib';

/** Signature of a zip archive's end-of-central-directory record. */
const END_OF_DIRECTORY = 0x06054b50;
/** Signature of an entry of the central directory. */
const DIRECTORY_ENTRY = 0x02014b50;
/** Signature of an entry's local header. */
const LOCAL_HEADER = 0x04034b50;
/** The end record is 22 bytes and may be followed by a comment of up to 64 KiB. */
const END_SEARCH = 22 + 0xffff;

/**
 * Reads one file out of a zip archive held in memory, such as the
 * browser's `omni.ja`; `null` when the archive has no entry of that name.
 *
 * Only what Firefox's own archives use is read: stored and deflated
 * entries, no zip64, no encryption.
 *
 * @throws {Error} when the archive is not a zip archive this reads.
 */
export function readArchiveEntry(archive: Buffer, name: string): Buffer | null {
  let end = archive.length - 22;
  const stop = Math.max(0, archive.length - END_SEARCH);
  while (end >= stop && archive.readUInt32LE(end) !== END_OF_DIRECTORY) {
    end -= 1;
  }
  if (end < stop) {
    throw new Error('the archive has no end-of-central-directory record');
  }
  const entries = archive.readUInt16LE(end + 10);
  let at = archive.readUInt32LE(end + 16);
  for (let index = 0; index < entries; index += 1) {
    if (at + 46 > archive.length || archive.readUInt32LE(at) !== DIRECTORY_ENTRY) {
      throw new Error(`the central directory is broken at offset ${at}`);
    }
    const method = archive.readUInt16LE(at + 10);
    const compressed = archive.readUInt32LE(at + 20);
    const nameLength = archive.readUInt16LE(at + 28);
    const extraLength = archive.readUInt16LE(at + 30);
    const commentLength = archive.readUInt16LE(at + 32);
    const local = archive.readUInt32LE(at + 42);
    const entryName = archive.toString('utf8', at + 46, at + 46 + nameLength);
    if (entryName === name) {
      if (archive.readUInt32LE(local) !== LOCAL_HEADER) {
        throw new Error(`the entry ${name} has no local header at offset ${local}`);
      }
      const start =
        local + 30 + archive.readUInt16LE(local + 26) + archive.readUInt16LE(local + 28);
      const data = archive.subarray(start, start + compressed);
      if (method === 0) {
        return Buffer.from(data);
      }
      if (method === 8) {
        return inflateRawSync(data);
      }
      throw new Error(`the entry ${name} is stored with method ${method}, which is not read`);
    }
    at += 46 + nameLength + extraLength + commentLength;
  }
  return null;
}
