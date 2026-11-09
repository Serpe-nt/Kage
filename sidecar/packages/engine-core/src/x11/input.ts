import type { InputDevice, KeyLevels, Lending, PointerButton } from '../behavior/device.js';

import { X11Connection, X11Error } from './connection.js';
import {
  DEAD_KEYS,
  DEAD_KEYSYMS,
  KEYSYM_LEVEL3,
  KEYSYM_SHIFT,
  NAMED_KEYSYMS,
  keysymsOf,
  unicodeKeysym,
} from './keysyms.js';

/** Event types of an XTEST `FakeInput` request. */
const FAKE = { keyPress: 2, keyRelease: 3, buttonPress: 4, buttonRelease: 5, motion: 6 } as const;

/** X button numbers. The wheel is buttons 4 to 7: one press and release per notch. */
const BUTTON: Readonly<Record<PointerButton, number>> = { left: 1, middle: 2, right: 3 };
const WHEEL = { up: 4, down: 5, left: 6, right: 7 } as const;

/**
 * How long applications are given to read a changed keyboard mapping
 * before the remapped key is pressed, and before it is taken away again.
 */
const REMAP_SETTLE_MS = 60;

/**
 * How long applications are given to read the keys lent before a text,
 * before its first key: spent once, before the plan's clock starts, so it
 * takes nothing from a hold or a gap (browser-07).
 */
const PREPARE_SETTLE_MS = 200;

/** XKB's `UseExtension`, `SetMap` and the core keyboard's device spec. */
const XKB = { useExtension: 0, setMap: 9, useCoreKeyboard: 0x100 } as const;

/**
 * The connection that tells the display's clients their keyboard is new
 * (XKB's NewKeyboardNotify), and the extension's opcode on it.
 */
interface Announcer {
  readonly connection: X11Connection;
  readonly xkb: number;
}

/** The key code of the right Alt key on every X server that uses evdev key codes. */
const RIGHT_ALT_KEYCODE = 108;

/**
 * Key codes of the X server's evdev keyboard that a page hears with an
 * empty `KeyboardEvent.code`, in Chrome 155 and Camoufox 156 alike: the
 * only keys lent to characters before typing. Other key codes a layout
 * leaves empty are `F13` to `F24`, `Lang5`, `IntlRo` or `IntlYen` to a
 * page (measured for QA round three, browser-07), and a letter on such a
 * key would be a tell of its own.
 */
const CODELESS_KEYCODES: ReadonlySet<number> = new Set([
  8, 120, 149, 154, 168, 178, 183, 184, 217, 219, 222, 230,
]);

/** Where a key symbol sits on the keyboard. */
export interface KeyPosition {
  readonly keycode: number;
  readonly shift: boolean;
  readonly altGr: boolean;
}

/**
 * Finds key symbols in a keyboard mapping.
 *
 * A row holds the symbols of one key: plain and shifted for the first
 * group, the same for a second group, then the third and fourth level
 * (AltGr, AltGr with Shift) of the first group. Only the first group is
 * used: a persona types on one layout.
 */
export class Keymap {
  private readonly positions = new Map<number, KeyPosition>();
  /** A key code with no symbol at all, to lend to characters the layout lacks. */
  readonly spare: number | null;
  /**
   * The other key codes with no symbol that a page hears with no `code`,
   * highest first: each character a text needs and the layout lacks is
   * lent one of them before typing.
   */
  readonly spares: readonly number[];

  constructor(rows: readonly (readonly number[])[], minKeycode: number) {
    let spare: number | null = null;
    const empty: number[] = [];
    const levels = [
      { column: 0, shift: false, altGr: false },
      { column: 1, shift: true, altGr: false },
      { column: 4, shift: false, altGr: true },
      { column: 5, shift: true, altGr: true },
    ];
    // Unshifted positions win over shifted ones, and those over AltGr.
    for (const level of levels) {
      rows.forEach((row, index) => {
        const keysym = row[level.column] ?? 0;
        if (keysym !== 0 && !this.positions.has(keysym)) {
          this.positions.set(keysym, {
            keycode: minKeycode + index,
            shift: level.shift,
            altGr: level.altGr,
          });
        }
      });
    }
    rows.forEach((row, index) => {
      if (row.every((keysym) => keysym === 0)) {
        spare = minKeycode + index;
        empty.push(spare);
      }
    });
    this.spare = spare;
    this.spares = empty
      .filter((keycode) => keycode !== spare && CODELESS_KEYCODES.has(keycode))
      .reverse();
    // Layouts list AltGr twice: on the right Alt key and on a key no
    // keyboard has. The real key is the one a person presses.
    if (rows[RIGHT_ALT_KEYCODE - minKeycode]?.[0] === KEYSYM_LEVEL3) {
      this.positions.set(KEYSYM_LEVEL3, { keycode: RIGHT_ALT_KEYCODE, shift: false, altGr: false });
    }
  }

  /** Where a key symbol is; `null` when no key of the layout has it. */
  find(keysym: number): KeyPosition | null {
    return this.positions.get(keysym) ?? null;
  }

  /** Where a character is, trying each of its symbols. */
  findCharacter(character: string): KeyPosition | null {
    for (const keysym of keysymsOf(character)) {
      const position = this.find(keysym);
      if (position !== null) {
        return position;
      }
    }
    return null;
  }
}

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

/**
 * Mouse and keyboard of one X display, driven through the XTEST extension.
 *
 * Everything sent here reaches the browser as input from a device does:
 * the X server moves its pointer and delivers core and XInput events, and
 * the browser makes its own DOM events from them, with its own wheel
 * deltas, smooth scrolling, modifier state and key codes of the display's
 * keyboard layout.
 */
export class X11Input implements InputDevice {
  private keymap: Keymap;
  private rowWidth: number;
  /** Characters that currently sit on the spare key, by the key held down. */
  private lent: string | null = null;
  /** Characters `prepare` lent a key of their own, by character. */
  private readonly prepared = new Map<string, number>();
  /** How many lendings hold each of those characters. */
  private readonly holders = new Map<string, number>();
  /** Keys down, by the key code each went down on. */
  private readonly down = new Map<string, number>();
  /** The connection that announces a changed keyboard, once opened; `null` when it cannot be. */
  private announcer: Promise<Announcer | null> | null = null;

  private constructor(
    private readonly display: string,
    private readonly connection: X11Connection,
    private readonly xtest: number,
    rows: number[][],
    private readonly deadKeys: boolean,
  ) {
    this.keymap = new Keymap(rows, connection.setup.minKeycode);
    this.rowWidth = rows[0]?.length ?? 2;
  }

  /**
   * Opens the input of a display.
   *
   * @param options.deadKeys whether a letter the layout lacks is typed with
   *   its dead key (`´`, then `e`), as a person does: only for a browser
   *   that composes the two on this display (default true). Otherwise such
   *   a letter is lent a key like any character the layout lacks.
   * @throws {X11Error} when the display cannot be reached or has no XTEST extension.
   */
  static async open(
    display: string,
    options: { readonly deadKeys?: boolean } = {},
  ): Promise<X11Input> {
    const connection = await X11Connection.connect(display);
    try {
      const extension = await connection.queryExtension('XTEST');
      if (extension === null) {
        throw new X11Error(`the X server at ${display} has no XTEST extension`);
      }
      return new X11Input(
        display,
        connection,
        extension.majorOpcode,
        await connection.keyboardMapping(),
        options.deadKeys ?? true,
      );
    } catch (error) {
      connection.close();
      throw error;
    }
  }

  /** Size of the display in pixels. */
  get screen(): { width: number; height: number } {
    return { width: this.connection.setup.width, height: this.connection.setup.height };
  }

  /** Reads the keyboard mapping again, after the layout of the display changed. */
  async reloadKeymap(): Promise<void> {
    const rows = await this.connection.keyboardMapping();
    this.keymap = new Keymap(rows, this.connection.setup.minKeycode);
    this.rowWidth = rows[0]?.length ?? 2;
  }

  private fake(type: number, detail: number, x = 0, y = 0): void {
    const body = Buffer.alloc(32);
    body[0] = type;
    body[1] = detail;
    body.writeInt16LE(x, 20);
    body.writeInt16LE(y, 22);
    // Minor opcode 2 is FakeInput.
    this.connection.send(this.xtest, 2, body);
  }

  pointerPosition(): Promise<{ x: number; y: number }> {
    return this.connection.queryPointer();
  }

  pointerMove(x: number, y: number): Promise<void> {
    const { width, height } = this.screen;
    // Absolute motion, kept on the screen: a pointer cannot leave it.
    this.fake(
      FAKE.motion,
      0,
      Math.max(0, Math.min(width - 1, Math.round(x))),
      Math.max(0, Math.min(height - 1, Math.round(y))),
    );
    return Promise.resolve();
  }

  buttonDown(button: PointerButton): Promise<void> {
    this.fake(FAKE.buttonPress, BUTTON[button]);
    return Promise.resolve();
  }

  buttonUp(button: PointerButton): Promise<void> {
    this.fake(FAKE.buttonRelease, BUTTON[button]);
    return Promise.resolve();
  }

  wheelNotch(direction: 'up' | 'down' | 'left' | 'right'): Promise<void> {
    this.fake(FAKE.buttonPress, WHEEL[direction]);
    this.fake(FAKE.buttonRelease, WHEEL[direction]);
    return Promise.resolve();
  }

  levelsOf(key: string): KeyLevels {
    const position = this.position(key);
    if (position !== null) {
      return {
        shift: position.shift,
        altGr: position.altGr,
        onLayout: true,
        keycode: position.keycode,
        compose: null,
      };
    }
    const compose = this.composed(key);
    if (compose !== null) {
      return { shift: false, altGr: false, onLayout: true, keycode: null, compose };
    }
    // A character the layout lacks is on the key `prepare` lent it, or
    // else lent the one spare key when it is pressed.
    const keycode = this.prepared.get(key) ?? this.keymap.spare;
    return { shift: false, altGr: false, onLayout: false, keycode, compose: null };
  }

  /**
   * The dead key and the letter that make a character the layout has no
   * key for (`é`: `´` and `e`), where the layout has both; `null` otherwise.
   */
  private composed(key: string): { dead: string; base: string } | null {
    if (!this.deadKeys || NAMED_KEYSYMS[key] !== undefined || DEAD_KEYSYMS[key] !== undefined) {
      return null;
    }
    const [base, mark, ...rest] = key.normalize('NFD');
    const dead = mark === undefined ? undefined : DEAD_KEYS[mark];
    if (base === undefined || dead === undefined || rest.length > 0) {
      return null;
    }
    if (this.keymap.find(dead.keysym) === null || this.keymap.findCharacter(base) === null) {
      return null;
    }
    return { dead: dead.name, base };
  }

  /**
   * Lends each character of the text that the layout has neither a key
   * nor a dead key for a spare key of its own, and waits once for the
   * applications to read the changed mapping, all before the text's first
   * key: a press of such a character is then as prompt as any other.
   * Characters beyond the spare keys there are borrow the one spare key
   * when they are pressed, as before. A character another call holds a key
   * for already is held once more, on the same key.
   */
  async prepare(text: string): Promise<Lending> {
    const taken = new Set(this.prepared.values());
    const free = this.keymap.spares.filter((keycode) => !taken.has(keycode));
    const changes = new Map<number, number>();
    const characters: string[] = [];
    for (const character of new Set(text)) {
      const holders = this.holders.get(character);
      if (holders !== undefined) {
        this.holders.set(character, holders + 1);
        characters.push(character);
        continue;
      }
      if (this.levelsOf(character).onLayout) {
        continue;
      }
      const keycode = free.shift();
      if (keycode === undefined) {
        break;
      }
      changes.set(keycode, unicodeKeysym(character));
      this.prepared.set(character, keycode);
      this.holders.set(character, 1);
      characters.push(character);
    }
    const lending: Lending = { characters };
    if (changes.size > 0) {
      try {
        await this.remap(changes);
        await sleep(PREPARE_SETTLE_MS);
      } catch (error) {
        // What this call lent goes back before the caller hears of it.
        await this.unprepare(lending).catch(() => undefined);
        throw error;
      }
    }
    return lending;
  }

  /**
   * Gives back what one `prepare` lent, once applications have read the
   * releases. A key another call still holds stays lent and is left alone,
   * down or up: only a key no call holds any more is taken back, and a
   * key still down on it comes up first.
   */
  async unprepare(lending: Lending): Promise<void> {
    const freed = new Map<string, number>();
    for (const character of lending.characters) {
      const holders = this.holders.get(character) ?? 0;
      if (holders > 1) {
        this.holders.set(character, holders - 1);
        continue;
      }
      this.holders.delete(character);
      const keycode = this.prepared.get(character);
      if (keycode !== undefined) {
        freed.set(character, keycode);
      }
    }
    if (freed.size === 0) {
      return;
    }
    const keycodes = new Set(freed.values());
    for (const [key, keycode] of [...this.down]) {
      if (keycodes.has(keycode)) {
        this.release(key, keycode);
      }
    }
    for (const character of freed.keys()) {
      this.prepared.delete(character);
    }
    await this.connection.sync();
    await sleep(REMAP_SETTLE_MS);
    await this.remap(new Map([...keycodes].map((keycode) => [keycode, 0])));
    // Applications read the keyboard again now, not while the next step's input comes.
    await sleep(PREPARE_SETTLE_MS);
  }

  /**
   * Gives key codes a new symbol (0: none), one request a key (rewriting
   * the keys between them would let the server rebuild their levels), and
   * waits until the server has them.
   */
  private async remap(changes: ReadonlyMap<number, number>): Promise<void> {
    for (const [keycode, keysym] of changes) {
      this.connection.changeKeyboardMapping(
        keycode,
        Array.from({ length: this.rowWidth }, (_unused, column) => (column < 2 ? keysym : 0)),
      );
    }
    await this.connection.sync();
    await this.announce();
  }

  /**
   * Tells the display's clients that their keyboard is new, as a keyboard
   * plugged in does (XKB's NewKeyboardNotify), so that each reads the key
   * map again with what was just lent in it. Chrome reads it only then, or
   * at a core MappingNotify, which the server sends no client that uses
   * XKB without asking for its map events, as Chrome does: a key lent
   * after Chrome's first key on the display was named `"\u0000"`, and only
   * the keys lent before that first key were named right (QA round four
   * typing-01). Asking XKB for a smaller key code range than the
   * keyboard's announces a new keyboard; the range is asked for again as it
   * was, so nothing of the keyboard changes. It goes on a connection of its
   * own, so that this one keeps the core MappingNotify it hears now. A
   * display without XKB, or one that refuses, announces nothing.
   */
  private async announce(): Promise<void> {
    this.announcer ??= this.openAnnouncer();
    const announcer = await this.announcer;
    if (announcer === null) {
      return;
    }
    const { minKeycode, maxKeycode } = this.connection.setup;
    try {
      for (const max of [maxKeycode - 1, maxKeycode]) {
        const body = Buffer.alloc(32);
        body.writeUInt16LE(XKB.useCoreKeyboard, 0);
        body[6] = minKeycode;
        body[7] = max;
        announcer.connection.send(announcer.xkb, XKB.setMap, body);
      }
      await announcer.connection.sync();
    } catch {
      // A keyboard announced or not, the keys are lent: typing goes on.
    }
  }

  /** Opens the connection that announces a changed keyboard; `null` without XKB. */
  private async openAnnouncer(): Promise<Announcer | null> {
    let connection: X11Connection | null = null;
    try {
      connection = await X11Connection.connect(this.display);
      const extension = await connection.queryExtension('XKEYBOARD');
      if (extension === null) {
        connection.close();
        return null;
      }
      const version = Buffer.alloc(4);
      version.writeUInt16LE(1, 0);
      version.writeUInt16LE(0, 2);
      const reply = await connection.request(extension.majorOpcode, XKB.useExtension, version);
      if (reply[1] !== 1) {
        connection.close();
        return null;
      }
      return { connection, xkb: extension.majorOpcode };
    } catch {
      connection?.close();
      return null;
    }
  }

  private position(key: string): KeyPosition | null {
    const named = NAMED_KEYSYMS[key] ?? DEAD_KEYSYMS[key];
    if (named !== undefined) {
      return this.keymap.find(named);
    }
    const codePoint = key.codePointAt(0);
    if (codePoint === undefined || String.fromCodePoint(codePoint) !== key) {
      throw new X11Error(`not a key this keyboard knows: ${JSON.stringify(key)}`);
    }
    return this.keymap.findCharacter(key);
  }

  private modifier(keysym: number, name: string): number {
    const position = this.keymap.find(keysym);
    if (position === null) {
      throw new X11Error(`the keyboard of this display has no ${name} key`);
    }
    return position.keycode;
  }

  async keyDown(key: string): Promise<void> {
    if (key === 'Shift') {
      this.fake(FAKE.keyPress, this.modifier(KEYSYM_SHIFT, 'Shift'));
      return;
    }
    if (key === 'AltGraph') {
      this.fake(FAKE.keyPress, this.modifier(KEYSYM_LEVEL3, 'AltGr'));
      return;
    }
    const keycode = this.position(key)?.keycode ?? this.prepared.get(key);
    if (keycode !== undefined) {
      this.press(key, keycode);
      return;
    }
    // No key of the layout produces this character: lend it a spare key
    // for as long as it is held.
    const spare = this.keymap.spare;
    if (spare === null || NAMED_KEYSYMS[key] !== undefined || DEAD_KEYSYMS[key] !== undefined) {
      throw new X11Error(`the keyboard of this display cannot produce ${JSON.stringify(key)}`);
    }
    if (this.lent !== null) {
      await this.keyUp(this.lent);
    }
    const keysym = unicodeKeysym(key);
    this.connection.changeKeyboardMapping(
      spare,
      Array.from({ length: this.rowWidth }, (_unused, column) => (column < 2 ? keysym : 0)),
    );
    await this.connection.sync();
    await this.announce();
    await sleep(REMAP_SETTLE_MS);
    this.lent = key;
    this.press(key, spare);
  }

  /**
   * Releases a key on the key code it went down on, whatever the keyboard
   * says of it now: a key lent for a text and given back since (a typing
   * that was stopped) still comes up.
   */
  async keyUp(key: string): Promise<void> {
    if (key === 'Shift') {
      this.fake(FAKE.keyRelease, this.modifier(KEYSYM_SHIFT, 'Shift'));
      return;
    }
    if (key === 'AltGraph') {
      this.fake(FAKE.keyRelease, this.modifier(KEYSYM_LEVEL3, 'AltGr'));
      return;
    }
    const spare = this.keymap.spare;
    if (this.lent === key && spare !== null) {
      this.release(key, spare);
      this.lent = null;
      await sleep(REMAP_SETTLE_MS);
      this.connection.changeKeyboardMapping(
        spare,
        Array.from({ length: this.rowWidth }, () => 0),
      );
      await this.connection.sync();
      return;
    }
    const keycode = this.down.get(key) ?? this.position(key)?.keycode ?? this.prepared.get(key);
    if (keycode !== undefined) {
      this.release(key, keycode);
    }
  }

  /** Sends a key's press, and remembers the key code it went down on. */
  private press(key: string, keycode: number): void {
    this.fake(FAKE.keyPress, keycode);
    this.down.set(key, keycode);
  }

  private release(key: string, keycode: number): void {
    this.fake(FAKE.keyRelease, keycode);
    this.down.delete(key);
  }

  /** Waits until the server has handled everything sent, and reports a refused request. */
  flush(): Promise<void> {
    return this.connection.sync();
  }

  async close(): Promise<void> {
    this.connection.close();
    const announcer = await this.announcer;
    announcer?.connection.close();
  }
}
