/**
 * Process entry point of the contract tool: `node dist/bin.js <command>`.
 * Everything testable lives in `cli.ts`.
 */
import { existsSync, readFileSync, writeFileSync } from 'node:fs';

import { runCli } from './cli.js';

process.exitCode = runCli(process.argv.slice(2), {
  read: (path) => (existsSync(path) ? readFileSync(path, 'utf8') : null),
  write: (path, text) => {
    writeFileSync(path, text);
  },
  out: (line) => process.stdout.write(`${line}\n`),
  err: (line) => process.stderr.write(`${line}\n`),
});
