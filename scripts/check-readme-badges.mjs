#!/usr/bin/env node
// The README's badges say what the repository says (docs/runbooks/repo.md,
// "Badges"):
//
//   node scripts/check-readme-badges.mjs
//
// The two static badges are read from files a pull request changes: the
// toolchain from rust-toolchain.toml, the licence from the workspace's
// Cargo.toml. A bump that forgets the README fails here. The other three
// (CI, the latest release, the coverage figure on the gh-pages branch) must
// be there, pointing at this repository.
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

/** The repository the README's badges point at. */
export const REPOSITORY = 'Serpe-nt/Kage';

/** A text as a part of a shields.io static badge's path: `-` is `--`, `_` is `__`, a space `%20`. */
export function shieldsText(text) {
  return encodeURIComponent(text.replaceAll('-', '--').replaceAll('_', '__'));
}

/** The pinned toolchain of a rust-toolchain.toml. */
export function toolchainOf(toml) {
  const match = /^\s*channel\s*=\s*"([^"]+)"/m.exec(toml);
  if (!match) throw new Error('rust-toolchain.toml names no channel');
  return match[1];
}

/** The licence expression of the workspace's Cargo.toml (`[workspace.package]`). */
export function licenceOf(toml) {
  const section = /^\[workspace\.package\]\s*$([\s\S]*?)(?=^\[|(?![\s\S]))/m.exec(toml);
  const match = section && /^\s*license\s*=\s*"([^"]+)"/m.exec(section[1]);
  if (!match) throw new Error('Cargo.toml has no license in [workspace.package]');
  return match[1];
}

/** Every badge image the README must show, by name. */
export function expectedBadges(toolchain, licence) {
  const endpoint = encodeURIComponent(
    `https://raw.githubusercontent.com/${REPOSITORY}/gh-pages/coverage.json`,
  );
  return {
    ci: `https://github.com/${REPOSITORY}/actions/workflows/ci.yaml/badge.svg?branch=main`,
    release: `https://img.shields.io/github/v/release/${REPOSITORY}?sort=semver&display_name=tag`,
    licence: `https://img.shields.io/badge/licence-${shieldsText(licence)}-blue`,
    toolchain: `https://img.shields.io/badge/rust-${shieldsText(toolchain)}-orange`,
    coverage: `https://img.shields.io/endpoint?url=${endpoint}`,
  };
}

/** What is wrong with the README's badges; empty when nothing is. */
export function problems(readme, toolchainToml, cargoToml) {
  const expected = expectedBadges(toolchainOf(toolchainToml), licenceOf(cargoToml));
  const images = new Set(
    [...readme.matchAll(/!\[[^\]]*\]\(([^)\s]+)\)/g)].map((match) => match[1]),
  );
  const found = [];
  for (const [name, url] of Object.entries(expected)) {
    if (!images.has(url)) {
      found.push(`the ${name} badge is not ${url}`);
    }
  }
  for (const url of images) {
    if (/img\.shields\.io\/badge\/(rust|licence)-/.test(url) && !Object.values(expected).includes(url)) {
      found.push(`a static badge says something else than the repository: ${url}`);
    }
  }
  return found;
}

function main() {
  const found = problems(
    readFileSync('README.md', 'utf8'),
    readFileSync('rust-toolchain.toml', 'utf8'),
    readFileSync('Cargo.toml', 'utf8'),
  );
  if (found.length > 0) {
    for (const problem of found) console.error(`error: README.md: ${problem}`);
    process.exit(1);
  }
  console.log('README.md: the five badges say what the repository says');
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) {
  main();
}
