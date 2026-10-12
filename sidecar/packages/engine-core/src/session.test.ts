import { describe, expect, it } from 'vitest';

import { entriesToGoBack, plainAddress, type TabHistory } from './session.js';

const site = (path: string) => `https://www.example.org${path}`;
const plain = (path: string) => plainAddress(site(path)) ?? '';

/** What a page says of its tab: by default one that began with the open page. */
const tab = (change: Partial<TabHistory> = {}): TabHistory => {
  const before = change.before ?? [];
  return { api: true, before, listed: before.length + 1, total: before.length + 1, ...change };
};

describe('the address a link is compared by', () => {
  it('is the origin, the path without the slash it ends on, and the query', () => {
    expect(plainAddress('https://www.example.org/a/b?x=1&y=2#part')).toBe(
      'https://www.example.org/a/b?x=1&y=2',
    );
    expect(plainAddress('https://www.example.org/a/b/')).toBe('https://www.example.org/a/b');
    expect(plainAddress('https://www.example.org/')).toBe('https://www.example.org');
    expect(plainAddress('HTTPS://WWW.Example.org:443/a')).toBe('https://www.example.org/a');
    expect(plainAddress('http://localhost:8088/site/?page=2')).toBe(
      'http://localhost:8088/site?page=2',
    );
    // Two addresses of one page compare equal, two pages do not.
    expect(plain('/site/')).toBe(plain('/site#team'));
    expect(plain('/site?page=2')).not.toBe(plain('/site'));
    expect(plainAddress('https://blog.example.org/a')).not.toBe(plain('/a'));
  });

  it('is nothing for what is no address', () => {
    expect(plainAddress('not an address')).toBeNull();
    expect(plainAddress('')).toBeNull();
    expect(plainAddress(null)).toBeNull();
    expect(plainAddress(undefined)).toBeNull();
  });
});

describe('how far back in its tab a persona goes for a page', () => {
  const wanted = plain('/listing');

  it('goes as far as the browser names the page, nearest first', () => {
    expect(entriesToGoBack(tab({ before: [site('/listing')] }), wanted, 3, true)).toBe(1);
    expect(entriesToGoBack(tab({ before: [site('/listing'), site('/a')] }), wanted, 3, true)).toBe(
      2,
    );
    expect(
      entriesToGoBack(
        tab({ before: [site('/listing'), site('/a'), site('/listing'), site('/b')] }),
        wanted,
        10,
        true,
      ),
    ).toBe(2);
    // The address as the tab has it: with a fragment, with a slash at its end.
    expect(entriesToGoBack(tab({ before: [site('/listing/#top')] }), wanted, 1, true)).toBe(1);
    // The browser's word is enough: what the session saw does not matter.
    expect(entriesToGoBack(tab({ before: [site('/listing')] }), wanted, 3, false)).toBe(1);
  });

  it('does not move when the browser names every entry within reach and none is the page', () => {
    // Not in the tab at all.
    expect(entriesToGoBack(tab({ before: [site('/a'), site('/b')] }), wanted, 3, true)).toBe(0);
    expect(entriesToGoBack(tab(), wanted, 3, true)).toBe(0);
    // In the tab, and further back than the persona may go.
    const far = tab({ before: [site('/listing'), site('/a'), site('/b')] });
    expect(entriesToGoBack(far, wanted, 2, true)).toBe(0);
    expect(entriesToGoBack(far, wanted, 3, true)).toBe(3);
    // Entries nobody names lie behind the ones within reach: out of reach too.
    expect(
      entriesToGoBack(
        tab({ before: [site('/a'), site('/b'), site('/c')], listed: 4, total: 6 }),
        wanted,
        3,
        true,
      ),
    ).toBe(0);
    // Nobody may go back at all.
    expect(entriesToGoBack(tab({ before: [site('/listing')] }), wanted, 0, true)).toBe(0);
  });

  it('goes and looks where an entry has no name, and only for a page the tab has shown', () => {
    // An entry whose address the browser keeps back.
    const kept = tab({ before: [null, site('/a')] });
    expect(entriesToGoBack(kept, wanted, 3, true)).toBe(2);
    expect(entriesToGoBack(kept, wanted, 3, false)).toBe(0);
    // Within a reach of one the browser names the entry, and it is another page.
    expect(entriesToGoBack(kept, wanted, 1, true)).toBe(0);
    expect(entriesToGoBack(tab({ before: [site('/a'), null] }), wanted, 1, true)).toBe(1);
    // A name behind one that is kept back: as far as the name, the other is looked at on the way.
    expect(entriesToGoBack(tab({ before: [site('/listing'), null] }), wanted, 3, false)).toBe(2);
    // Entries of another origin are not listed at all: the tab has more than the browser names.
    const foreign = tab({ before: [], listed: 1, total: 3 });
    expect(entriesToGoBack(foreign, wanted, 3, true)).toBe(2);
    expect(entriesToGoBack(foreign, wanted, 1, true)).toBe(1);
    expect(entriesToGoBack(foreign, wanted, 3, false)).toBe(0);
    // Past the ones it names, as far as the reach goes.
    expect(
      entriesToGoBack(tab({ before: [site('/a')], listed: 2, total: 4 }), wanted, 3, true),
    ).toBe(3);
    expect(
      entriesToGoBack(tab({ before: [site('/a')], listed: 2, total: 3 }), wanted, 3, true),
    ).toBe(2);
  });

  it('goes entry by entry in a browser that lists nothing, when there is anything to go back to', () => {
    const blind = (total: number): TabHistory => ({ api: false, before: [], listed: 0, total });
    expect(entriesToGoBack(blind(4), wanted, 3, true)).toBe(3);
    expect(entriesToGoBack(blind(2), wanted, 1, true)).toBe(1);
    // The tab began with the open page, or never showed the one wanted.
    expect(entriesToGoBack(blind(1), wanted, 3, true)).toBe(0);
    expect(entriesToGoBack(blind(4), wanted, 3, false)).toBe(0);
    expect(entriesToGoBack(blind(4), wanted, 0, true)).toBe(0);
  });
});
