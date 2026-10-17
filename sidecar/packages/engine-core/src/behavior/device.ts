/**
 * What the behavior engine needs from the outside: a mouse and a keyboard
 * to send input to, a page to look at, and a clock.
 *
 * The device is the display's own (XTEST on the context's X server): the
 * browser gets input the way it gets it from hardware, on both engines
 * alike. The page surface is the one thing each engine supplies.
 */

/** A point in pixels. */
export interface Point {
  readonly x: number;
  readonly y: number;
}

/** A rectangle in pixels of the viewport. */
export interface Box {
  readonly x: number;
  readonly y: number;
  readonly width: number;
  readonly height: number;
}

/** A mouse button. */
export type PointerButton = 'left' | 'middle' | 'right';

/** Which modifier keys the layout needs for a key, and whether it has the key at all. */
export interface KeyLevels {
  readonly shift: boolean;
  readonly altGr: boolean;
  /** False when no key of the layout produces the character: the device then lends it a key. */
  readonly onLayout: boolean;
}

/** The mouse and the keyboard of one display. Coordinates are pixels of the screen. */
export interface InputDevice {
  /** Size of the screen in pixels. */
  readonly screen: { readonly width: number; readonly height: number };
  pointerPosition(): Promise<Point>;
  pointerMove(x: number, y: number): Promise<void>;
  buttonDown(button: PointerButton): Promise<void>;
  buttonUp(button: PointerButton): Promise<void>;
  /** Turns the wheel by one notch. */
  wheelNotch(direction: 'up' | 'down' | 'left' | 'right'): Promise<void>;
  /**
   * What a key needs on this keyboard. A key is one character, or a name
   * as `KeyboardEvent.key` writes it (`Enter`, `Backspace`, `Shift`, …).
   */
  levelsOf(key: string): KeyLevels;
  /** Presses a key. Modifiers are keys too and are pressed by the caller. */
  keyDown(key: string): Promise<void>;
  keyUp(key: string): Promise<void>;
  /** Waits until everything sent has been handled, and throws for anything the display refused. */
  flush(): Promise<void>;
  close(): Promise<void>;
}

/** Where the page is and how far it is scrolled. */
export interface SurfaceMetrics {
  /** The top left corner of the viewport on the screen. */
  readonly origin: Point;
  /** Size of the viewport. */
  readonly viewport: { readonly width: number; readonly height: number };
  readonly scrollY: number;
  /** Height of the whole document. */
  readonly scrollHeight: number;
}

/**
 * The page as the behavior engine sees it. Every method only reads, and
 * reads in a way the page cannot notice (the driver's isolated world).
 */
export interface PageSurface {
  metrics(): Promise<SurfaceMetrics>;
  /** Words of text inside the viewport right now. */
  visibleWords(): Promise<number>;
  /** The box of the first element a CSS selector matches, in viewport pixels; `null` without one. */
  boxOf(selector: string): Promise<Box | null>;
  /**
   * Waits until the page has drawn two frames, at most `limitMs`. A
   * browser hands a page its pointer moves and its wheel with the frames
   * it draws, and loses what came while it drew none.
   *
   * @returns false when the limit came first.
   */
  painted(limitMs: number): Promise<boolean>;
}

/** Time, injectable so that tests do not wait. */
export interface Clock {
  now(): number;
  sleep(ms: number): Promise<void>;
}

/** The real clock. */
export const systemClock: Clock = {
  now: () => performance.now(),
  sleep: (ms) => new Promise((resolve) => setTimeout(resolve, Math.max(0, ms))),
};
