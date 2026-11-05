// Tests of scripts/dev-secrets.sh: what an operator wrote is kept, whatever
// the file's last byte, and a second run changes nothing (F137, cli-05).

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const script = join(dirname(fileURLToPath(import.meta.url)), 'dev-secrets.sh');

/** Runs the script in a copy of the layout it expects, on this `.env`. */
function run(env) {
  const root = mkdtempSync(join(tmpdir(), 'dev-secrets-'));
  try {
    mkdirSync(join(root, 'scripts'));
    mkdirSync(join(root, 'deploy'));
    copyFileSync(script, join(root, 'scripts', 'dev-secrets.sh'));
    const file = join(root, 'deploy', '.env');
    if (env !== undefined) {
      writeFileSync(file, env);
    }
    execFileSync('bash', [join(root, 'scripts', 'dev-secrets.sh')], { stdio: 'pipe' });
    const first = readFileSync(file, 'utf8');
    execFileSync('bash', [join(root, 'scripts', 'dev-secrets.sh')], { stdio: 'pipe' });
    const second = readFileSync(file, 'utf8');
    return { first, second };
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

const keys = (text) =>
  text
    .split('\n')
    .filter((line) => line.length > 0)
    .map((line) => line.split('=')[0]);

test('a last line without its newline keeps its value and the new keys come on lines of their own', () => {
  const { first, second } = run('KAGE_EGRESS_MODE=direct');
  const lines = first.split('\n');
  assert.equal(lines[0], 'KAGE_EGRESS_MODE=direct');
  assert.deepEqual(keys(first), [
    'KAGE_EGRESS_MODE',
    'KAGE_POSTGRES_PASSWORD',
    'KAGE_SIDECAR_TOKEN',
    'KAGE_MASTER_KEY',
    'KAGE_GRAFANA_ADMIN_PASSWORD',
  ]);
  assert.ok(first.endsWith('\n'));
  // Run again: nothing to add, nothing changed.
  assert.equal(second, first);
});

test('a complete file is left as it is and a missing file is made', () => {
  const made = run(undefined).first;
  assert.equal(keys(made).length, 4);
  const { first } = run(made);
  assert.equal(first, made);
  // An empty file gets no blank line in front.
  assert.ok(!run('').first.startsWith('\n'));
});
