import { mkdir, readFile, rename, writeFile } from 'node:fs/promises';
import path from 'node:path';

import type { ProfilePreferences } from './launch-plan.js';

/** Directories of one persona below its profile directory. */
export interface ProfilePaths {
  /** Chrome's user data directory: cookies, storage, history, cache. */
  readonly userData: string;
  /** `HOME` of the browser process: its certificate database and desktop state. */
  readonly home: string;
  /** Chrome's own preferences file of the default profile. */
  readonly preferences: string;
}

/** Where the parts of a persona's profile live. */
export function profilePaths(profileDir: string): ProfilePaths {
  const userData = path.join(profileDir, 'user-data');
  return {
    userData,
    home: path.join(profileDir, 'home'),
    preferences: path.join(userData, 'Default', 'Preferences'),
  };
}

type JsonObject = Record<string, unknown>;

const isObject = (value: unknown): value is JsonObject =>
  typeof value === 'object' && value !== null && !Array.isArray(value);

/**
 * Merges Kage's keys into a profile's preferences and leaves every other
 * key as it is: a profile accrues state over a persona's life (spec §6.1
 * step 5), and only the keys Kage owns are ever overwritten.
 */
export function mergePreferences(existing: unknown, patch: JsonObject): JsonObject {
  const merged: JsonObject = isObject(existing) ? { ...existing } : {};
  for (const [key, value] of Object.entries(patch)) {
    merged[key] = isObject(value) ? mergePreferences(merged[key], value) : value;
  }
  return merged;
}

async function readJson(file: string): Promise<unknown> {
  try {
    return JSON.parse(await readFile(file, 'utf8'));
  } catch {
    // A missing file is a new profile; an unreadable one is a file Chrome
    // itself would reset.
    return {};
  }
}

/**
 * Creates the directories of a profile and writes Kage's preference keys
 * into it. Must run while no browser has the profile open: Chrome writes
 * its preferences back when it exits.
 */
export async function prepareProfile(
  profileDir: string,
  preferences: ProfilePreferences,
): Promise<ProfilePaths> {
  const paths = profilePaths(profileDir);
  await mkdir(path.dirname(paths.preferences), { recursive: true, mode: 0o700 });
  await mkdir(paths.home, { recursive: true, mode: 0o700 });
  const merged = mergePreferences(await readJson(paths.preferences), { ...preferences });
  const temporary = `${paths.preferences}.kage`;
  await writeFile(temporary, JSON.stringify(merged), { mode: 0o600 });
  await rename(temporary, paths.preferences);
  return paths;
}
