import { describe, expect, it } from 'vitest';

import type { InputDevice } from './device.js';
import {
  GatedInput,
  InputGrant,
  InputRevoked,
  childGrant,
  currentGrant,
  withInputGrant,
} from './input-gate.js';

function recording(): { acts: string[]; device: InputDevice } {
  const acts: string[] = [];
  const note = (act: string) => {
    acts.push(act);
    return Promise.resolve();
  };
  return {
    acts,
    device: {
      screen: { width: 800, height: 600 },
      pointerPosition: () => Promise.resolve({ x: 1, y: 2 }),
      pointerMove: (x, y) => note(`move ${x},${y}`),
      buttonDown: (button) => note(`button down ${button}`),
      buttonUp: (button) => note(`button up ${button}`),
      wheelNotch: (direction) => note(`wheel ${direction}`),
      levelsOf: () => ({ shift: true, altGr: false, onLayout: true, keycode: null, compose: null }),
      prepare: (text) => note(`prepare ${text}`).then(() => ({ characters: [] })),
      unprepare: () => note('unprepare'),
      keyDown: (key) => note(`key down ${key}`),
      keyUp: (key) => note(`key up ${key}`),
      flush: () => note('flush'),
      close: () => note('close'),
    },
  };
}

const later = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

describe('the input gate (QA round four, input-01)', () => {
  it('acts for work outside any grant, and for work whose grant is active', async () => {
    const { acts, device } = recording();
    const gated = new GatedInput(device);
    expect(currentGrant()).toBeNull();
    await gated.pointerMove(1, 1);
    await withInputGrant(new InputGrant(), async () => {
      await gated.pointerMove(2, 2);
      await gated.buttonDown('left');
      await gated.buttonUp('left');
      await gated.wheelNotch('down');
      await gated.prepare('ä');
      await gated.keyDown('a');
      await gated.keyUp('a');
      await gated.unprepare({ characters: [] });
      await gated.flush();
    });
    await gated.close();
    expect(acts).toEqual([
      'move 1,1',
      'move 2,2',
      'button down left',
      'button up left',
      'wheel down',
      'prepare ä',
      'key down a',
      'key up a',
      'unprepare',
      'flush',
      'close',
    ]);
  });

  it('refuses every act of a revoked grant before it acts, and still answers reads', async () => {
    const { acts, device } = recording();
    const gated = new GatedInput(device);
    const grant = new InputGrant();
    await grant.revoke();
    expect(grant.revoked).toBe(true);
    await withInputGrant(grant, async () => {
      for (const act of [
        () => gated.pointerMove(3, 3),
        () => gated.buttonDown('right'),
        () => gated.wheelNotch('up'),
        () => gated.prepare('x'),
        () => gated.keyDown('Enter'),
      ]) {
        await expect(act()).rejects.toBeInstanceOf(InputRevoked);
      }
      expect(gated.screen).toEqual({ width: 800, height: 600 });
      expect(await gated.pointerPosition()).toEqual({ x: 1, y: 2 });
      expect(gated.levelsOf('A').shift).toBe(true);
      // Releases of what it does not hold do nothing; giving back is never refused.
      await gated.keyUp('Enter');
      await gated.buttonUp('right');
      await gated.unprepare({ characters: [] });
    });
    expect(acts).toEqual(['unprepare']);
  });

  it('lets up what a grant still holds when it is revoked, and nothing of it afterwards', async () => {
    const { acts, device } = recording();
    const gated = new GatedInput(device);
    const grant = new InputGrant();
    await withInputGrant(grant, async () => {
      await gated.keyDown('Shift');
      await gated.keyDown('a');
      await gated.buttonDown('left');
    });
    acts.length = 0;
    await grant.revoke();
    expect(acts).toEqual(['key up a', 'key up Shift', 'button up left', 'flush']);
    // Revoking again does nothing; a late release of the work does nothing.
    await grant.revoke();
    await withInputGrant(grant, async () => {
      await gated.keyUp('a');
      await gated.buttonUp('left');
    });
    expect(acts).toEqual(['key up a', 'key up Shift', 'button up left', 'flush']);
    // A grant that never pressed anything has nothing to let up.
    const idle = new InputGrant();
    await idle.revoke();
    expect(acts).toHaveLength(4);
  });

  it('goes with everything the work started, however late it runs', async () => {
    const { acts, device } = recording();
    const gated = new GatedInput(device);
    const request = new InputGrant();
    let late: Promise<void> = Promise.resolve();
    let step: InputGrant | null = null;
    await withInputGrant(request, async () => {
      step = childGrant();
      expect(step.parent).toBe(request);
      await withInputGrant(step, async () => {
        // Work the step started and did not wait for: it runs after the
        // request has ended.
        late = later(30).then(() => gated.keyDown('ArrowDown'));
        await gated.keyDown('ArrowDown');
        await gated.keyUp('ArrowDown');
      });
    });
    // The request ends: its grant, and the step's inside it, are revoked.
    await request.revoke();
    expect(step).not.toBeNull();
    expect((step as unknown as InputGrant).active).toBe(false);
    await expect(late).rejects.toBeInstanceOf(InputRevoked);
    // The next request's work acts under a grant of its own.
    await withInputGrant(new InputGrant(), () => gated.keyDown('b'));
    expect(acts).toEqual(['key down ArrowDown', 'key up ArrowDown', 'key down b']);
  });
});
