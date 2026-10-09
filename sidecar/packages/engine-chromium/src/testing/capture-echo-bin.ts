/**
 * Process entry point of the browser capture (follow-up F5):
 *
 *   node dist/testing/capture-echo-bin.js --host echo.kage.test --port 8443 \
 *     --ca ca.pem --out-dir tests/fixtures/fingerprints/browser
 *
 * `scripts/capture-browser-fingerprint.sh` starts the echo and calls this.
 */
import { writeFileSync } from 'node:fs';
import path from 'node:path';

import { captureEcho, captureFileName, serializeCapture } from './capture-echo.js';

const args = process.argv.slice(2);
const option = (name: string): string => {
  const index = args.indexOf(`--${name}`);
  const value = index >= 0 ? args[index + 1] : undefined;
  if (value === undefined) {
    throw new Error(`missing --${name}`);
  }
  return value;
};

try {
  const capture = await captureEcho({
    echoHost: option('host'),
    echoPort: Number(option('port')),
    caFile: option('ca'),
  });
  const file = path.join(option('out-dir'), captureFileName(capture));
  writeFileSync(file, serializeCapture(capture));
  process.stdout.write(
    `${file}\n  Chrome ${capture.browser_version}\n  ja4    ${capture.ja4}\n  akamai ${capture.akamai}\n  header order of a typed navigation:\n    ${capture.headers.join(', ')}\n`,
  );
} catch (error) {
  process.stderr.write(
    `capture failed: ${error instanceof Error ? error.message : String(error)}\n`,
  );
  process.exitCode = 1;
}
