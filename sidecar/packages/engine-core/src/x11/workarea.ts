import { X11Connection } from './connection.js';

/** Predefined atoms of the X protocol. */
const ATOM = { atom: 4, cardinal: 6, window: 33 } as const;

/** A rectangle of the screen in pixels. */
export interface WorkArea {
  readonly x: number;
  readonly y: number;
  readonly width: number;
  readonly height: number;
}

const cardinals = (values: readonly number[]): Buffer => {
  const data = Buffer.alloc(values.length * 4);
  values.forEach((value, index) => data.writeUInt32LE(value >>> 0, index * 4));
  return data;
};

const HINTS = [
  '_NET_SUPPORTING_WM_CHECK',
  '_NET_WM_NAME',
  '_NET_SUPPORTED',
  '_NET_WORKAREA',
  '_NET_NUMBER_OF_DESKTOPS',
  '_NET_CURRENT_DESKTOP',
  '_NET_DESKTOP_GEOMETRY',
  '_NET_DESKTOP_VIEWPORT',
] as const;
type Hint = (typeof HINTS)[number];

/**
 * Tells the clients of a display which part of its screen is free for
 * windows, the way a desktop with a panel or a taskbar does (the
 * `_NET_WORKAREA` hint of the window manager specification).
 *
 * A bare X server has no such hint, and a browser on it reports a work
 * area equal to the screen: `screen.availHeight === screen.height`, which
 * a desktop with a panel never shows. Nothing here manages windows; it
 * only publishes what a window manager publishes about the desktop. The
 * hint lives as long as the returned connection is open.
 */
export async function announceWorkArea(
  display: string,
  area: WorkArea,
): Promise<{ close(): void }> {
  const connection = await X11Connection.connect(display);
  try {
    const { root, width, height } = connection.setup;
    const atoms = new Map<string, number>();
    for (const name of [...HINTS, 'UTF8_STRING']) {
      atoms.set(name, await connection.internAtom(name));
    }
    const atom = (name: Hint | 'UTF8_STRING') => atoms.get(name) ?? 0;
    const check = connection.createHiddenWindow();
    for (const window of [root, check]) {
      connection.changeProperty(
        window,
        atom('_NET_SUPPORTING_WM_CHECK'),
        ATOM.window,
        32,
        cardinals([check]),
      );
    }
    connection.changeProperty(
      check,
      atom('_NET_WM_NAME'),
      atom('UTF8_STRING'),
      8,
      Buffer.from('kage', 'utf8'),
    );
    connection.changeProperty(
      root,
      atom('_NET_SUPPORTED'),
      ATOM.atom,
      32,
      cardinals(HINTS.map((name) => atom(name))),
    );
    const set = (name: Hint, values: readonly number[]) => {
      connection.changeProperty(root, atom(name), ATOM.cardinal, 32, cardinals(values));
    };
    set('_NET_NUMBER_OF_DESKTOPS', [1]);
    set('_NET_CURRENT_DESKTOP', [0]);
    set('_NET_DESKTOP_GEOMETRY', [width, height]);
    set('_NET_DESKTOP_VIEWPORT', [0, 0]);
    set('_NET_WORKAREA', [area.x, area.y, area.width, area.height]);
    await connection.sync();
    return {
      close: () => {
        connection.close();
      },
    };
  } catch (error) {
    connection.close();
    throw error;
  }
}
