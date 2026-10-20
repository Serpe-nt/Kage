// The README badge check (node --test).
import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  expectedBadges,
  licenceOf,
  problems,
  shieldsText,
  toolchainOf,
} from './check-readme-badges.mjs';

const TOOLCHAIN = '[toolchain]\nchannel = "1.99.0"\nprofile = "minimal"\n';
const CARGO =
  '[workspace]\nmembers = ["crates/*"]\n\n[workspace.package]\nversion = "2.0.0"\nlicense = "MIT OR Apache-2.0"\n\n[workspace.dependencies]\nfoo = { version = "=1.0.0", license = "nonsense" }\n';

const readme = (badges) =>
  `# Kage\n\n${Object.entries(badges)
    .map(([name, url]) => `[![${name}](${url})](https://example.org/${name})`)
    .join('\n')}\n`;

test('the toolchain and the licence are read from their files', () => {
  assert.equal(toolchainOf(TOOLCHAIN), '1.99.0');
  assert.equal(licenceOf(CARGO), 'MIT OR Apache-2.0');
  assert.throws(() => toolchainOf('[toolchain]\n'), /no channel/);
  assert.throws(() => licenceOf('[package]\nlicense = "MIT"\n'), /no license/);
});

test('a static badge escapes as shields.io reads it', () => {
  assert.equal(shieldsText('MIT OR Apache-2.0'), 'MIT%20OR%20Apache--2.0');
  assert.equal(shieldsText('1.99.0'), '1.99.0');
  assert.equal(shieldsText('a_b'), 'a__b');
});

test('a README with the five badges as the repository says passes', () => {
  const expected = expectedBadges('1.99.0', 'MIT OR Apache-2.0');
  assert.deepEqual(problems(readme(expected), TOOLCHAIN, CARGO), []);
  assert.equal(
    expected.coverage,
    'https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2FSerpe-nt%2FKage%2Fgh-pages%2Fcoverage.json',
  );
});

test('a toolchain bump that forgot the README fails, and so does a missing badge', () => {
  const old = expectedBadges('1.98.0', 'MIT OR Apache-2.0');
  const found = problems(readme(old), TOOLCHAIN, CARGO);
  assert.equal(found.length, 2, found.join('\n'));
  assert.match(found[0], /the toolchain badge is not .*rust-1\.99\.0-orange/);
  assert.match(found[1], /something else than the repository: .*rust-1\.98\.0/);

  const { coverage: _coverage, ...four } = expectedBadges('1.99.0', 'MIT OR Apache-2.0');
  assert.deepEqual(problems(readme(four), TOOLCHAIN, CARGO), [
    `the coverage badge is not ${expectedBadges('1.99.0', 'x').coverage}`,
  ]);
});
