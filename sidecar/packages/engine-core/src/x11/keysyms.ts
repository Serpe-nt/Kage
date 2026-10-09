/**
 * Key symbols of the X protocol, as far as typing text and pressing named
 * keys needs them.
 */

/** Key symbols of keys that have a name instead of a character, by `KeyboardEvent.key`. */
export const NAMED_KEYSYMS: Readonly<Record<string, number>> = {
  Backspace: 0xff08,
  Tab: 0xff09,
  Enter: 0xff0d,
  Escape: 0xff1b,
  Delete: 0xffff,
  Home: 0xff50,
  ArrowLeft: 0xff51,
  ArrowUp: 0xff52,
  ArrowRight: 0xff53,
  ArrowDown: 0xff54,
  PageUp: 0xff55,
  PageDown: 0xff56,
  End: 0xff57,
  Insert: 0xff63,
  F1: 0xffbe,
  F2: 0xffbf,
  F3: 0xffc0,
  F4: 0xffc1,
  F5: 0xffc2,
  F6: 0xffc3,
  F7: 0xffc4,
  F8: 0xffc5,
  F9: 0xffc6,
  F10: 0xffc7,
  F11: 0xffc8,
  F12: 0xffc9,
  Shift: 0xffe1,
  Control: 0xffe3,
  Alt: 0xffe9,
  AltGraph: 0xfe03,
  ' ': 0x20,
};

/** `Shift_L`, the key that selects the second level of a key. */
export const KEYSYM_SHIFT = 0xffe1;
/** `ISO_Level3_Shift`, the AltGr key that selects the third level. */
export const KEYSYM_LEVEL3 = 0xfe03;

/**
 * Characters outside Latin-1 that keyboard layouts name with a symbol of
 * their own instead of the Unicode form.
 */
const LEGACY: Readonly<Record<string, number>> = {
  '€': 0x20ac,
  œ: 0x13bd,
  Œ: 0x13bc,
  š: 0x1b9,
  Š: 0x1a9,
  ž: 0x1be,
  Ž: 0x1ae,
  Ÿ: 0x13be,
};

/**
 * The key symbols that may stand for one character, most common first:
 * the Latin-1 symbol (equal to the code point), a legacy symbol, and the
 * Unicode form every code point has.
 */
export function keysymsOf(character: string): number[] {
  const codePoint = character.codePointAt(0);
  if (codePoint === undefined) {
    return [];
  }
  const candidates: number[] = [];
  if (codePoint === 0x0a || codePoint === 0x0d) {
    return [0xff0d];
  }
  if (codePoint === 0x09) {
    return [0xff09];
  }
  if ((codePoint >= 0x20 && codePoint <= 0x7e) || (codePoint >= 0xa0 && codePoint <= 0xff)) {
    candidates.push(codePoint);
  }
  const legacy = LEGACY[character];
  if (legacy !== undefined) {
    candidates.push(legacy);
  }
  if (codePoint >= 0x100) {
    candidates.push(0x01000000 + codePoint);
  }
  return candidates;
}

/** The key symbol a spare key is given to produce a character no key of the layout has. */
export function unicodeKeysym(character: string): number {
  const [first] = keysymsOf(character);
  return first ?? 0x20;
}
