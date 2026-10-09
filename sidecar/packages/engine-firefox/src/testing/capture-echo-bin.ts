/**
 * Process entry point of the browser capture for the Firefox engine
 * (follow-up F5 of docs/02-plan.md). `scripts/capture-browser-fingerprint.sh
 * firefox` starts the echo and calls this.
 */
import { execFile } from 'node:child_process';
import { mkdir } from 'node:fs/promises';
import { promisify } from 'node:util';

import { captureMain } from '@kage/engine-core/testing';

import { createEngine, profilePaths } from '../index.js';

const run = promisify(execFile);

await captureMain(
  {
    engine: 'firefox',
    // What a Firefox persona claims in production (spec §6.1): the
    // capture is of the browser as targets see it.
    claimedOs: 'windows',
    screen: { min_width: 1600, max_width: 1920, min_height: 900, max_height: 1080 },
    createEngine,
    // Firefox keeps the authorities a user added in the certificate
    // database of the profile.
    trust: async (profileDir, caFile) => {
      const database = profilePaths(profileDir).userData;
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
