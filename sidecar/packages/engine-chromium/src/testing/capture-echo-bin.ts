/**
 * Process entry point of the browser capture for the Chromium engine
 * (follow-up F5 of docs/02-plan.md). `scripts/capture-browser-fingerprint.sh`
 * starts the echo and calls this.
 */
import { execFile } from 'node:child_process';
import { mkdir } from 'node:fs/promises';
import path from 'node:path';
import { promisify } from 'node:util';

import { captureMain } from '@kage/engine-core/testing';

import { createEngine } from '../index.js';
import { profilePaths } from '../profile.js';

const run = promisify(execFile);

await captureMain(
  {
    engine: 'chromium',
    claimedOs: 'linux',
    screen: { min_width: 1920, max_width: 1920, min_height: 1080, max_height: 1080 },
    createEngine,
    // Chrome on Linux reads user-trusted authorities from the NSS database
    // in its home directory.
    trust: async (profileDir, caFile) => {
      const database = path.join(profilePaths(profileDir).home, '.pki', 'nssdb');
      await mkdir(database, { recursive: true, mode: 0o700 });
      await run('certutil', ['-d', `sql:${database}`, '-N', '--empty-password']);
      await run('certutil', [
        '-d',
        `sql:${database}`,
        '-A',
        '-t',
        'C,,',
        '-n',
        'kage-fingerprint-echo',
        '-i',
        caFile,
      ]);
    },
  },
  process.argv.slice(2),
);
