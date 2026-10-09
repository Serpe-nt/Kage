import { afterAll, beforeAll, describe, expect, it } from 'vitest';

import { DisplayPool, startXvfb, type VirtualDisplay } from '../display.js';

import { OPCODE, X11Connection, X11Error, parseSetup, socketPathOf } from './connection.js';
import { Keymap, X11Input } from './input.js';
import { KEYSYM_LEVEL3, KEYSYM_SHIFT, NAMED_KEYSYMS, keysymsOf, unicodeKeysym } from './keysyms.js';
import { announceWorkArea } from './workarea.js';

/**
 * The X11 client against a real X server: an Xvfb with a German keyboard,
 * as a persona of a German proxy gets it.
 */

const pool = new DisplayPool();
let number: number;
let display: VirtualDisplay;

beforeAll(async () => {
  number = pool.acquire();
  display = await startXvfb(number, { width: 1280, height: 800, depth: 24 }, { layout: 'de' });
});

afterAll(async () => {
  await display.stop();
  pool.release(number);
});

describe('the connection', () => {
  it('finds the socket of a local display and refuses anything else', () => {
    expect(socketPathOf(':100')).toBe('/tmp/.X11-unix/X100');
    expect(socketPathOf(':7.0')).toBe('/tmp/.X11-unix/X7');
    expect(() => socketPathOf('host:0')).toThrow(X11Error);
    expect(() => socketPathOf('')).toThrow(/not a local display name/);
  });

  it('reads the answer to the setup: incomplete, refused, accepted', () => {
    expect(parseSetup(Buffer.alloc(4))).toBeNull();
    const refused = Buffer.alloc(16);
    refused[1] = 6;
    refused.writeUInt16LE(2, 6);
    refused.write('denied', 8, 'latin1');
    expect(parseSetup(refused.subarray(0, 12))).toBeNull();
    expect(() => parseSetup(refused)).toThrow(/refused the connection: denied/);
    const empty = Buffer.alloc(8 + 36);
    empty[0] = 1;
    empty.writeUInt16LE(9, 6);
    expect(() => parseSetup(empty)).toThrow(/described no screen/);
  });

  it('says so when there is no such display', async () => {
    const error: unknown = await X11Connection.connect(':3999').catch((thrown: unknown) => thrown);
    expect(error).toBeInstanceOf(X11Error);
    expect((error as X11Error).message).toContain('cannot connect to the X server at :3999');
    await expect(X11Input.open(':3999')).rejects.toBeInstanceOf(X11Error);
  });

  it('describes the screen, answers requests, and reports what it refuses', async () => {
    const connection = await X11Connection.connect(display.name);
    expect(connection.setup).toMatchObject({
      width: 1280,
      height: 800,
      minKeycode: 8,
      maxKeycode: 255,
    });
    expect(connection.setup.root).toBeGreaterThan(0);
    expect(await connection.queryExtension('XTEST')).toMatchObject({
      majorOpcode: expect.any(Number) as number,
    });
    expect(await connection.queryExtension('NO-SUCH-EXTENSION')).toBeNull();
    const atom = await connection.internAtom('_KAGE_TEST');
    expect(await connection.internAtom('_KAGE_TEST')).toBe(atom);
    expect(connection.newResourceId()).not.toBe(connection.newResourceId());

    // A request with a reply that the server refuses rejects with its error code (3: no such window).
    const window = Buffer.alloc(4);
    window.writeUInt32LE(0x7fff_fff0, 0);
    await expect(connection.request(OPCODE.queryPointer, 0, window)).rejects.toMatchObject({
      code: 3,
    });
    // A request without a reply shows its refusal at the next sync, once.
    connection.changeProperty(0x7fff_fff0, atom, 6, 32, Buffer.alloc(4));
    await expect(connection.sync()).rejects.toBeInstanceOf(X11Error);
    await expect(connection.sync()).resolves.toBeUndefined();

    connection.close();
    connection.close();
    expect(() => {
      connection.send(OPCODE.getInputFocus, 0);
    }).toThrow(/closed/);
    await expect(connection.sync()).rejects.toBeInstanceOf(X11Error);
  });

  it('publishes the work area of the desktop for as long as it is held', async () => {
    const hint = await announceWorkArea(display.name, { x: 0, y: 32, width: 1280, height: 768 });
    const connection = await X11Connection.connect(display.name);
    const read = async (name: string) => {
      const body = Buffer.alloc(20);
      body.writeUInt32LE(connection.setup.root, 0);
      body.writeUInt32LE(await connection.internAtom(name), 4);
      body.writeUInt32LE(64, 16);
      // GetProperty (20) of any type.
      const reply = await connection.request(20, 0, body);
      const count = reply.readUInt32LE(16);
      return Array.from({ length: count }, (_unused, index) => reply.readUInt32LE(32 + index * 4));
    };
    expect(await read('_NET_WORKAREA')).toEqual([0, 32, 1280, 768]);
    expect(await read('_NET_DESKTOP_GEOMETRY')).toEqual([1280, 800]);
    expect(await read('_NET_SUPPORTING_WM_CHECK')).toHaveLength(1);
    expect(await read('_NET_SUPPORTED')).toContain(await connection.internAtom('_NET_WORKAREA'));
    hint.close();
    connection.close();
    await expect(
      announceWorkArea(':3998', { x: 0, y: 0, width: 1, height: 1 }),
    ).rejects.toBeInstanceOf(X11Error);
  });
});

describe('key symbols and keymaps', () => {
  it('names the symbols a character may have', () => {
    expect(keysymsOf('a')).toEqual([0x61]);
    expect(keysymsOf('ä')).toEqual([0xe4]);
    expect(keysymsOf('€')).toEqual([0x20ac, 0x0100_20ac]);
    expect(keysymsOf('ж')).toEqual([0x0100_0436]);
    expect(keysymsOf('\n')).toEqual([0xff0d]);
    expect(keysymsOf('\t')).toEqual([0xff09]);
    expect(keysymsOf('')).toEqual([]);
    expect(unicodeKeysym('ж')).toBe(0x0100_0436);
    expect(unicodeKeysym('')).toBe(0x20);
    expect(NAMED_KEYSYMS['Enter']).toBe(0xff0d);
  });

  it('finds keys by level, prefers the plain one, and knows a spare key', () => {
    const rows = [
      [0x61, 0x41, 0, 0, 0x40, 0],
      [0x62, 0x61, 0, 0, 0, 0x20ac],
      [0, 0, 0, 0, 0, 0],
      [KEYSYM_SHIFT, 0, 0, 0, 0, 0],
    ];
    const keymap = new Keymap(rows, 10);
    expect(keymap.find(0x61)).toEqual({ keycode: 10, shift: false, altGr: false });
    expect(keymap.find(0x41)).toEqual({ keycode: 10, shift: true, altGr: false });
    expect(keymap.find(0x40)).toEqual({ keycode: 10, shift: false, altGr: true });
    expect(keymap.findCharacter('€')).toEqual({ keycode: 11, shift: true, altGr: true });
    expect(keymap.findCharacter('ж')).toBeNull();
    expect(keymap.find(KEYSYM_LEVEL3)).toBeNull();
    expect(keymap.spare).toBe(12);
    expect(new Keymap([[0x61]], 8).spare).toBeNull();
  });
});

describe('the input of a display', () => {
  it('moves the pointer where it is told, and never off the screen', async () => {
    const input = await X11Input.open(display.name);
    expect(input.screen).toEqual({ width: 1280, height: 800 });
    await input.pointerMove(300.4, 200.6);
    await input.flush();
    expect(await input.pointerPosition()).toEqual({ x: 300, y: 201 });
    await input.pointerMove(5000, -20);
    await input.flush();
    expect(await input.pointerPosition()).toEqual({ x: 1279, y: 0 });
    for (const button of ['left', 'middle', 'right'] as const) {
      await input.buttonDown(button);
      await input.buttonUp(button);
    }
    for (const direction of ['up', 'down', 'left', 'right'] as const) {
      await input.wheelNotch(direction);
    }
    await input.flush();
    await input.close();
    await input.close();
  });

  it('knows a German keyboard: Shift for capitals, AltGr for @ and €, Z where Y is', async () => {
    const input = await X11Input.open(display.name);
    expect(input.levelsOf('z')).toEqual({ shift: false, altGr: false, onLayout: true });
    expect(input.levelsOf('Z')).toEqual({ shift: true, altGr: false, onLayout: true });
    expect(input.levelsOf('ü')).toEqual({ shift: false, altGr: false, onLayout: true });
    expect(input.levelsOf('@')).toEqual({ shift: false, altGr: true, onLayout: true });
    expect(input.levelsOf('€')).toEqual({ shift: false, altGr: true, onLayout: true });
    expect(input.levelsOf('Enter')).toEqual({ shift: false, altGr: false, onLayout: true });
    expect(input.levelsOf('ж')).toEqual({ shift: false, altGr: false, onLayout: false });
    expect(() => input.levelsOf('NoSuchKey')).toThrow(/not a key this keyboard knows/);
    // Every kind of key goes down and up without the server refusing anything.
    for (const key of ['Shift', 'AltGraph', 'a', 'Enter', 'Backspace', ' ']) {
      await input.keyDown(key);
      await input.keyUp(key);
    }
    await input.flush();
    await input.reloadKeymap();
    expect(input.levelsOf('y').onLayout).toBe(true);
    await input.close();
  });

  it('lends a spare key to a character the layout does not have, and takes it back', async () => {
    const input = await X11Input.open(display.name);
    const connection = await X11Connection.connect(display.name);
    const free = async () =>
      (await connection.keyboardMapping()).filter((row) => row.every((keysym) => keysym === 0))
        .length;
    const before = await free();
    await input.keyDown('ж');
    expect(await free()).toBe(before - 1);
    // A second such character takes the same key over.
    await input.keyDown('ю');
    expect(await free()).toBe(before - 1);
    await input.keyUp('ю');
    await input.keyUp('ж');
    expect(await free()).toBe(before);
    await input.flush();
    connection.close();
    await input.close();
  });
});
