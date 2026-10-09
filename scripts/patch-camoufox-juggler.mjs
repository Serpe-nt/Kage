#!/usr/bin/env node
// Backports one upstream fix into the pinned Camoufox build (ADR 0010):
// "fix(juggler): evaluate() no longer grants user activation", Camoufox pull
// request 779, in the builds from v156.0.1-beta.32 on.
//
//   node scripts/patch-camoufox-juggler.mjs <camoufox directory> \
//       --from <sha256 of the build's omni.ja> --to <sha256 after the backport>
//   node scripts/patch-camoufox-juggler.mjs <camoufox directory> --to <sha256> --check
//
// The driver inside the browser (Juggler) ran every evaluation of a page as
// user input: `windowUtils.setHandlingUserInput(true)` and
// `document.notifyUserGestureActivation()` in `evaluateScript` and
// `evaluateFunction` of `chrome/juggler/content/content/Runtime.js`. A page
// nobody touched then read `navigator.userActivation` as active from the
// first read on (spec §10.2). Either call alone grants it; both are taken
// out, as upstream did. Upstream's new switch for the popup blocker needs
// the browser's C++ and is not part of this.
//
// `omni.ja` is a zip archive and this entry is stored in it uncompressed.
// The four statements are replaced by text of the same length, so every
// offset in the archive stays what it was, and the entry's CRC-32 is
// written anew in its two places. Nothing is repacked: 260 of the archive's
// 43 153 155 bytes differ afterwards.
//
// The tool only ever turns the one pinned archive into the one pinned
// result. Any other input is refused, so it cannot outlive the build it
// was written for: when the pin moves to a build that has the fix, this
// file and its two checksums are deleted (docs/runbooks/engine-firefox.md).
// With `--check` it changes nothing and says whether the archive is the
// patched one.
import { createHash } from 'node:crypto';
import { readFileSync, renameSync, statSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { crc32 } from 'node:zlib';

const ENTRY = 'chrome/juggler/content/content/Runtime.js';

/** What is replaced, how often it occurs, and the statement that takes its place. */
const EDITS = [
  {
    needle: 'this._domWindow ? this._domWindow.windowUtils.setHandlingUserInput(true) : null;',
    statement: 'null;',
    note: 'Kage: a read is no user input',
    count: 2,
  },
  {
    needle: 'this._domWindow.document.notifyUserGestureActivation();',
    statement: 'void 0;',
    note: 'Kage: a read is no user gesture',
    count: 2,
  },
];

const fail = (message) => {
  console.error(`error: ${message}`);
  process.exit(1);
};

const sha256 = (buffer) => createHash('sha256').update(buffer).digest('hex');

/** The statement and a comment, padded to the length of what they replace. */
function sameLength(edit) {
  const room = edit.needle.length - edit.statement.length - ' /*  */'.length;
  if (room < edit.note.length) {
    throw new Error(`no room for the note in place of: ${edit.needle}`);
  }
  return `${edit.statement} /* ${edit.note.padEnd(room, ' ')} */`;
}

/** Finds a stored entry: where its data and its two checksums are. */
function locate(archive, name) {
  // The end-of-central-directory record, searched from the end.
  let end = archive.length - 22;
  while (end >= 0 && archive.readUInt32LE(end) !== 0x06054b50) {
    end -= 1;
  }
  if (end < 0) {
    throw new Error('the archive has no end-of-central-directory record');
  }
  let at = archive.readUInt32LE(end + 16);
  for (let index = archive.readUInt16LE(end + 10); index > 0; index -= 1) {
    if (archive.readUInt32LE(at) !== 0x02014b50) {
      throw new Error(`the central directory is broken at offset ${at}`);
    }
    const nameLength = archive.readUInt16LE(at + 28);
    if (archive.toString('latin1', at + 46, at + 46 + nameLength) === name) {
      const local = archive.readUInt32LE(at + 42);
      const size = archive.readUInt32LE(at + 24);
      if (archive.readUInt16LE(at + 10) !== 0 || archive.readUInt32LE(at + 20) !== size) {
        throw new Error(`${name} is compressed in this archive`);
      }
      if ((archive.readUInt16LE(at + 8) & 0x08) !== 0) {
        throw new Error(`${name} keeps its checksum behind its data in this archive`);
      }
      if (archive.readUInt32LE(local) !== 0x04034b50) {
        throw new Error(`${name} has no local header`);
      }
      const data =
        local + 30 + archive.readUInt16LE(local + 26) + archive.readUInt16LE(local + 28);
      return { data, size, centralCrc: at + 16, localCrc: local + 14 };
    }
    at += 46 + nameLength + archive.readUInt16LE(at + 30) + archive.readUInt16LE(at + 32);
  }
  throw new Error(`the archive has no ${name}`);
}

/**
 * The archive with the backport applied, as a new buffer.
 *
 * @throws when the entry is not what the pinned build holds.
 */
export function backport(original) {
  const archive = Buffer.from(original);
  const entry = locate(archive, ENTRY);
  const before = archive.subarray(entry.data, entry.data + entry.size);
  const stored = archive.readUInt32LE(entry.centralCrc);
  if (crc32(before) !== stored || archive.readUInt32LE(entry.localCrc) !== stored) {
    throw new Error(`${ENTRY} does not match its checksum`);
  }
  let text = before.toString('latin1');
  for (const edit of EDITS) {
    const parts = text.split(edit.needle);
    if (parts.length - 1 !== edit.count) {
      throw new Error(
        `expected ${edit.count} of "${edit.needle}" in ${ENTRY}, found ${parts.length - 1}`,
      );
    }
    text = parts.join(sameLength(edit));
  }
  const after = Buffer.from(text, 'latin1');
  if (after.length !== entry.size) {
    throw new Error('the entry changed its size');
  }
  after.copy(archive, entry.data);
  const checksum = crc32(after);
  archive.writeUInt32LE(checksum, entry.centralCrc);
  archive.writeUInt32LE(checksum, entry.localCrc);
  return archive;
}

function main(argv) {
  const [directory, ...rest] = argv;
  const option = (name) => {
    const index = rest.indexOf(`--${name}`);
    return index >= 0 ? rest[index + 1] : undefined;
  };
  const from = option('from');
  const to = option('to');
  const check = rest.includes('--check');
  if (directory === undefined || to === undefined || (!check && from === undefined)) {
    fail(
      'usage: patch-camoufox-juggler.mjs <camoufox directory> --from <sha256> --to <sha256> [--check]',
    );
  }
  const file = path.join(directory, 'omni.ja');
  let archive;
  try {
    archive = readFileSync(file);
  } catch (error) {
    fail(`cannot read ${file}: ${error.message}`);
  }
  const found = sha256(archive);
  if (found === to) {
    console.log(`${file} holds the Juggler backport already (${to})`);
    return;
  }
  if (check) {
    fail(`${file} is ${found}, not the build with the Juggler backport (${to})`);
  }
  if (found !== from) {
    fail(
      `${file} is ${found}: neither the pinned build's archive (${from}) nor the patched one (${to}). This tool changes the pinned build only.`,
    );
  }
  let patched;
  try {
    patched = backport(archive);
  } catch (error) {
    fail(error.message);
  }
  const result = sha256(patched);
  if (result !== to) {
    fail(`the backport gave ${result}, the pin says ${to}: nothing was written`);
  }
  // Written beside the archive and moved over it, so that a browser never
  // finds half an archive.
  const beside = `${file}.kage-${process.pid}`;
  writeFileSync(beside, patched, { mode: statSync(file).mode });
  renameSync(beside, file);
  console.log(`${file}: Juggler backport applied (${from} -> ${to})`);
}

if (process.argv[1] !== undefined && path.resolve(process.argv[1]) === import.meta.filename) {
  main(process.argv.slice(2));
}
