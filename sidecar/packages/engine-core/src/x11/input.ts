import type { InputDevice, KeyLevels, PointerButton } from '../behavior/device.js';

import { X11Connection, X11Error } from './connection.js';
import { KEYSYM_LEVEL3, KEYSYM_SHIFT, NAMED_KEYSYMS, keysymsOf, unicodeKeysym } from './keysyms.js';

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

/** The key code of the right Alt key on every X server that uses evdev key codes. */
const RIGHT_ALT_KEYCODE = 108;

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

  constructor(rows: readonly (readonly number[])[], minKeycode: number) {
    let spare: number | null = null;
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
      }
    });
    this.spare = spare;
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

  private constructor(
    private readonly connection: X11Connection,
    private readonly xtest: number,
    rows: number[][],
  ) {
    this.keymap = new Keymap(rows, connection.setup.minKeycode);
    this.rowWidth = rows[0]?.length ?? 2;
  }

  /**
   * Opens the input of a display.
   *
   * @throws {X11Error} when the display cannot be reached or has no XTEST extension.
   */
  static async open(display: string): Promise<X11Input> {
    const connection = await X11Connection.connect(display);
    try {
      const extension = await connection.queryExtension('XTEST');
      if (extension === null) {
        throw new X11Error(`the X server at ${display} has no XTEST extension`);
      }
      return new X11Input(connection, extension.majorOpcode, await connection.keyboardMapping());
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
    return position === null
      ? { shift: false, altGr: false, onLayout: false }
      : { shift: position.shift, altGr: position.altGr, onLayout: true };
  }

  private position(key: string): KeyPosition | null {
    const named = NAMED_KEYSYMS[key];
    if (named !== undefined) {
      return this.keymap.find(named);
    }
    if ([...key].length !== 1) {
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
    const position = this.position(key);
    if (position !== null) {
      this.fake(FAKE.keyPress, position.keycode);
      return;
    }
    // No key of the layout produces this character: lend it a spare key
    // for as long as it is held.
    const spare = this.keymap.spare;
    if (spare === null || NAMED_KEYSYMS[key] !== undefined) {
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
    await sleep(REMAP_SETTLE_MS);
    this.lent = key;
    this.fake(FAKE.keyPress, spare);
  }

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
      this.fake(FAKE.keyRelease, spare);
      this.lent = null;
      await sleep(REMAP_SETTLE_MS);
      this.connection.changeKeyboardMapping(
        spare,
        Array.from({ length: this.rowWidth }, () => 0),
      );
      await this.connection.sync();
      return;
    }
    const position = this.position(key);
    if (position !== null) {
      this.fake(FAKE.keyRelease, position.keycode);
    }
  }

  /** Waits until the server has handled everything sent, and reports a refused request. */
  flush(): Promise<void> {
    return this.connection.sync();
  }

  close(): Promise<void> {
    this.connection.close();
    return Promise.resolve();
  }
}
