import {
  GatedInput,
  InputGrant,
  InputRevoked,
  withInputGrant,
  type InputDevice,
} from '@kage/engine-core';
import { describe, expect, it } from 'vitest';

import { pressPrint } from './context.js';

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
      pointerPosition: () => Promise.resolve({ x: 0, y: 0 }),
      pointerMove: () => note('move'),
      buttonDown: (button) => note(`button down ${button}`),
      buttonUp: (button) => note(`button up ${button}`),
      wheelNotch: (direction) => note(`wheel ${direction}`),
      levelsOf: () => ({
        shift: false,
        altGr: false,
        onLayout: true,
        keycode: null,
        compose: null,
      }),
      prepare: () => Promise.resolve({ characters: [] }),
      unprepare: () => Promise.resolve(),
      keyDown: (key) => note(`down ${key}`),
      keyUp: (key) => note(`up ${key}`),
      flush: () => note('flush'),
      close: () => Promise.resolve(),
    },
  };
}

const quick = () => Promise.resolve();

describe('the print shortcut of a Firefox page (QA round four input-01, review of #96)', () => {
  it('presses Control and p through the gate for a step whose grant is active', async () => {
    const { acts, device } = recording();
    await withInputGrant(new InputGrant(), () => pressPrint(new GatedInput(device), quick));
    expect(acts).toEqual(['down Control', 'down p', 'up p', 'up Control', 'flush']);
  });

  it('presses nothing for a pdf step whose grant was revoked, nor outside any request', async () => {
    const { acts, device } = recording();
    const grant = new InputGrant();
    await grant.revoke();
    await expect(
      withInputGrant(grant, () => pressPrint(new GatedInput(device), quick)),
    ).rejects.toBeInstanceOf(InputRevoked);
    await expect(pressPrint(new GatedInput(device), quick)).rejects.toBeInstanceOf(InputRevoked);
    expect(acts).toEqual([]);
  });

  it('presses no p once the grant is revoked between the keys, and leaves no key down', async () => {
    const { acts, device } = recording();
    const grant = new InputGrant();
    const printing = withInputGrant(grant, () =>
      pressPrint(new GatedInput(device), async (ms) => {
        if (ms === 60) {
          // The step's grace ends while the hand holds Control.
          await grant.revoke();
        }
      }),
    );
    await expect(printing).rejects.toBeInstanceOf(InputRevoked);
    expect(acts).toEqual(['down Control', 'up Control', 'flush']);
  });
});
