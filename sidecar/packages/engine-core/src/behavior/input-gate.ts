import { AsyncLocalStorage } from 'node:async_hooks';

import type { InputDevice, KeyLevels, Lending, Point, PointerButton } from './device.js';

/**
 * An act of the persona's mouse or keyboard asked for by work whose grant
 * was taken back: a step whose grace ended, a request that returned (QA
 * round four, input-01). It never reaches the display.
 */
export class InputRevoked extends Error {
  override readonly name = 'InputRevoked';
}

/**
 * The right of one piece of work to act through the persona's input device:
 * a request, and inside it a step of a flow. The work runs with its grant
 * (see {@link withInputGrant}), and everything it starts, however late it
 * runs, carries that grant along (the async context). Once revoked, no
 * move, press, button or wheel of that work reaches the display: the act
 * throws {@link InputRevoked} before it does anything. What the work still
 * held down is let up at the revocation, and nothing of it afterwards; the
 * grant of the next step or request is another, and is not touched.
 */
export class InputGrant {
  private revokedNow = false;
  /** The keys this grant pressed and has not let up. */
  readonly keysDown = new Set<string>();
  /** The buttons this grant pressed and has not let up. */
  readonly buttonsDown = new Set<PointerButton>();
  /** The device behind the gate this grant pressed through, to let up what it holds. */
  private heldOn: InputDevice | null = null;

  /** @param parent the grant of the work this one runs inside: revoking it revokes this one too. */
  constructor(readonly parent: InputGrant | null = null) {}

  /** Whether the work may still act: neither this grant nor one it runs inside was revoked. */
  get active(): boolean {
    return !this.revokedNow && (this.parent?.active ?? true);
  }

  /** Whether this grant itself was taken back. */
  get revoked(): boolean {
    return this.revokedNow;
  }

  /** Notes the device a press of this grant went to. */
  pressedOn(device: InputDevice): void {
    this.heldOn = device;
  }

  /**
   * Takes the grant back: from now on no act of its work reaches the
   * display. What it still holds down is let up, through the device behind
   * the gate (the gate refuses the revoked grant).
   */
  async revoke(): Promise<void> {
    if (this.revokedNow) {
      return;
    }
    this.revokedNow = true;
    const keys = [...this.keysDown].reverse();
    const buttons = [...this.buttonsDown];
    this.keysDown.clear();
    this.buttonsDown.clear();
    const device = this.heldOn;
    if (device === null || (keys.length === 0 && buttons.length === 0)) {
      return;
    }
    for (const key of keys) {
      await device.keyUp(key).catch(() => undefined);
    }
    for (const button of buttons) {
      await device.buttonUp(button).catch(() => undefined);
    }
    await device.flush().catch(() => undefined);
  }
}

const granted = new AsyncLocalStorage<InputGrant>();

/** The grant of the work running now; `null` outside any. */
export function currentGrant(): InputGrant | null {
  return granted.getStore() ?? null;
}

/**
 * Runs `work` with `grant`. The grant goes with everything `work` starts,
 * and outlives it: whoever ends the work revokes it.
 */
export function withInputGrant<T>(grant: InputGrant, work: () => Promise<T>): Promise<T> {
  return granted.run(grant, work);
}

/** A new grant inside the one of the work running now. */
export function childGrant(): InputGrant {
  return new InputGrant(currentGrant());
}

/**
 * The persona's input device, which acts only for work whose grant is
 * still active. Reads (the screen, the pointer's place, a key's levels)
 * are always answered. A revoked grant's move, button down, wheel notch,
 * key down and key lending throw {@link InputRevoked} and never act; its
 * key up and button up let up only what it still holds itself (the
 * revocation let up the rest), so that a late release never cuts a press
 * of the next step on the same key. Work outside any grant acts as before.
 */
export class GatedInput implements InputDevice {
  constructor(readonly device: InputDevice) {}

  get screen(): { readonly width: number; readonly height: number } {
    return this.device.screen;
  }

  private allowed(what: string): InputGrant | null {
    const grant = currentGrant();
    if (grant !== null && !grant.active) {
      throw new InputRevoked(
        `${what}: the step or request that asked for it has ended and acts no more`,
      );
    }
    return grant;
  }

  pointerPosition(): Promise<Point> {
    return this.device.pointerPosition();
  }

  async pointerMove(x: number, y: number): Promise<void> {
    this.allowed('a pointer move');
    await this.device.pointerMove(x, y);
  }

  async buttonDown(button: PointerButton): Promise<void> {
    const grant = this.allowed('a button press');
    grant?.buttonsDown.add(button);
    grant?.pressedOn(this.device);
    await this.device.buttonDown(button);
  }

  async buttonUp(button: PointerButton): Promise<void> {
    const grant = currentGrant();
    if (grant !== null && !grant.active && !grant.buttonsDown.has(button)) {
      return;
    }
    grant?.buttonsDown.delete(button);
    await this.device.buttonUp(button);
  }

  async wheelNotch(direction: 'up' | 'down' | 'left' | 'right'): Promise<void> {
    this.allowed('a wheel notch');
    await this.device.wheelNotch(direction);
  }

  levelsOf(key: string): KeyLevels {
    return this.device.levelsOf(key);
  }

  async prepare(text: string): Promise<Lending> {
    this.allowed('a keyboard made ready for a text');
    return this.device.prepare(text);
  }

  unprepare(lending: Lending): Promise<void> {
    // Giving back is never refused: a key kept lent would stay lent.
    return this.device.unprepare(lending);
  }

  async keyDown(key: string): Promise<void> {
    const grant = this.allowed(`a press of ${key}`);
    grant?.keysDown.add(key);
    grant?.pressedOn(this.device);
    await this.device.keyDown(key);
  }

  async keyUp(key: string): Promise<void> {
    const grant = currentGrant();
    if (grant !== null && !grant.active && !grant.keysDown.has(key)) {
      return;
    }
    grant?.keysDown.delete(key);
    await this.device.keyUp(key);
  }

  flush(): Promise<void> {
    return this.device.flush();
  }

  close(): Promise<void> {
    return this.device.close();
  }
}
