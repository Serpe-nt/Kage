/**
 * The address patterns of `wait_for url` (spec §10.5), compared by the
 * sidecar itself: a driver's own wait for an address also waits for a
 * `load` event, which a page that comes back out of the browser's
 * back/forward cache never fires (follow-up F21).
 *
 * - `**` is any characters, slashes included;
 * - `*` is any characters except `/`;
 * - `{a,b}` is one of the alternatives, which may hold wildcards and braces;
 * - everything else is itself. A `?` is a question mark: addresses have one.
 *
 * A pattern is compared with the whole address. One that names no scheme
 * says where it begins: with a `/` it is a path on whatever origin the page
 * is on, and anything else that does not start with `*` may begin at the
 * start of the address or after any `/` of it: `listing?page=2` finds
 * `https://example.org/shop/listing?page=2`, and `example.org/a` finds
 * `https://example.org/a`. The fragment of an address counts only for a
 * pattern that has a `#`.
 */

/** The start of an address that names its scheme. */
const WITH_SCHEME = /^[a-zA-Z][a-zA-Z0-9+.-]*:\/\//;
/** A scheme and a host and nothing after them. */
const ORIGIN_ONLY = /^[a-zA-Z][a-zA-Z0-9+.-]*:\/\/[^/?#*]+$/;
/** Any origin, as a regular expression. */
const ANY_ORIGIN = '[a-zA-Z][a-zA-Z0-9+.-]*://[^/?#]*';

const literal = (text: string) => text.replace(/[\\^$.*+?()[\]{}|]/g, '\\$&');

/** Which braces of a pattern have a partner: one without is a character like any other. */
function pairedBraces(characters: readonly string[]): Set<number> {
  const paired = new Set<number>();
  const open: number[] = [];
  for (const [index, character] of characters.entries()) {
    if (character === '{') {
      open.push(index);
    } else if (character === '}') {
      const start = open.pop();
      if (start !== undefined) {
        paired.add(start).add(index);
      }
    }
  }
  return paired;
}

/** Writes a glob as the source of a regular expression. */
function translate(glob: string): string {
  const characters = Array.from(glob);
  const paired = pairedBraces(characters);
  let source = '';
  let depth = 0;
  for (let index = 0; index < characters.length; index += 1) {
    const character = characters[index] ?? '';
    if (character === '*') {
      let stars = 1;
      while (characters[index + 1] === '*') {
        stars += 1;
        index += 1;
      }
      source += stars > 1 ? '.*' : '[^/]*';
    } else if (character === '{' && paired.has(index)) {
      source += '(?:';
      depth += 1;
    } else if (character === '}' && paired.has(index)) {
      source += ')';
      depth -= 1;
    } else if (character === ',' && depth > 0) {
      source += '|';
    } else {
      source += literal(character);
    }
  }
  return source;
}

/**
 * Compiles an address pattern.
 *
 * @returns a function that says whether an address, as the browser reports it, matches.
 */
export function urlMatcher(glob: string): (url: string) => boolean {
  let prefix = '';
  let pattern = glob;
  if (glob.startsWith('/')) {
    prefix = ANY_ORIGIN;
  } else if (WITH_SCHEME.test(glob)) {
    // The browser writes an address without a path with its slash.
    pattern = ORIGIN_ONLY.test(glob) ? `${glob}/` : glob;
  } else if (!glob.startsWith('*')) {
    prefix = '(?:.*/)?';
  }
  const expression = new RegExp(`^${prefix}${translate(pattern)}$`, 's');
  const withFragment = glob.includes('#');
  return (url) => expression.test(withFragment ? url : (url.split('#')[0] ?? ''));
}
