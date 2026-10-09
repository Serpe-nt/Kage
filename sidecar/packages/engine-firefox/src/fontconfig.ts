import path from 'node:path';

import type { ClaimedOs } from '@kage/contract';
import { EngineError } from '@kage/engine-core';

/** The line of the build's file that names its fonts relative to the working directory. */
const RELATIVE_FONTS = '<dir prefix="cwd">fonts</dir>';
const HINTING_ON = '<edit name="hinting" mode="assign"><bool>true</bool></edit>';
const HINT_FULL = '<edit name="hintstyle" mode="assign"><const>hintfull</const></edit>';

/**
 * The fontconfig file a persona's browser runs with.
 *
 * It is the build's own file for the claimed system, with two changes:
 *
 * - The font directory is absolute. The build's file names it relative to
 *   the working directory, and the launcher points at a directory that
 *   does not exist in this build; with either, no file is read at all and
 *   `serif`, `sans-serif` and `monospace` all become one font.
 * - For a Windows or a macOS claim hinting is off. Those systems lay text
 *   out with the fonts' own fractional advances; whole-pixel advances are
 *   what FreeType's hinting gives and would show in every text
 *   measurement. A Linux claim keeps the build's setting.
 *
 * @throws {EngineError} `internal` when the build's file no longer has the
 *   lines this function changes: a bump of the build must be looked at.
 */
export function personaFontconfig(os: ClaimedOs, shipped: string, installDir: string): string {
  if (!shipped.includes(RELATIVE_FONTS)) {
    throw new EngineError(
      'internal',
      "the Camoufox build's fontconfig file changed: review personaFontconfig before using this build",
    );
  }
  let text = shipped.replace(RELATIVE_FONTS, `<dir>${path.join(installDir, 'fonts')}</dir>`);
  if (os !== 'linux') {
    if (!text.includes(HINTING_ON) || !text.includes(HINT_FULL)) {
      throw new EngineError(
        'internal',
        "the Camoufox build's fontconfig file no longer sets hinting the way personaFontconfig expects",
      );
    }
    text = text
      .replace(HINTING_ON, '<edit name="hinting" mode="assign"><bool>false</bool></edit>')
      .replace(HINT_FULL, '<edit name="hintstyle" mode="assign"><const>hintnone</const></edit>');
  }
  return text;
}
