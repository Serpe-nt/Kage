// The coverage gate's counting and the badge it writes (node --test).
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { badgeOf, colourOf, countByCrate } from './check-rust-coverage.mjs';

const file = (filename, count, covered) => ({ filename, summary: { lines: { count, covered } } });

test('only the files under crates/<name>/src count, for their crate', () => {
  const summary = {
    data: [
      {
        files: [
          file('/target/ci/kage/crates/kage-core/src/lib.rs', 100, 90),
          file('C:\\work\\crates\\kage-core\\src\\domain\\proxy.rs', 50, 50),
          file('/target/ci/kage/crates/kage-core/tests/pool.rs', 400, 0),
          file('/target/ci/kage/crates/kage/src/main.rs', 10, 5),
          file('/home/dev/.cargo/registry/src/wreq/src/lib.rs', 999, 0),
          file('/target/ci/kage/crates/unknown/src/lib.rs', 7, 7),
        ],
      },
    ],
  };
  const totals = countByCrate(summary, ['kage', 'kage-core', 'kage-store']);
  assert.deepEqual(Object.fromEntries(totals), {
    kage: { count: 10, covered: 5 },
    'kage-core': { count: 150, covered: 140 },
    'kage-store': { count: 0, covered: 0 },
  });
});

test('the badge is the whole workspace, one decimal, never more than was measured', () => {
  const totals = new Map([
    ['kage', { count: 6526, covered: 5729 }],
    ['kage-core', { count: 60035, covered: 57994 }],
  ]);
  // 63723 of 66561 lines: 95.736…, written 95.7.
  assert.deepEqual(badgeOf(totals), {
    schemaVersion: 1,
    label: 'coverage',
    message: '95.7 %',
    color: 'brightgreen',
  });
  // 89.99 % is not rounded up to 90.0.
  const below = badgeOf(new Map([['kage', { count: 10_000, covered: 8999 }]]));
  assert.equal(below.message, '89.9 %');
  assert.equal(below.color, 'yellowgreen');
  assert.equal(badgeOf(new Map([['kage', { count: 3, covered: 3 }]])).message, '100.0 %');
});

test('a summary without a line has no figure', () => {
  assert.throws(() => badgeOf(new Map([['kage', { count: 0, covered: 0 }]])), /no line/);
});

test('the colours change at 90, 80 and 70', () => {
  assert.deepEqual(
    [100, 90, 89.9, 80, 79.9, 70, 69.9, 0].map(colourOf),
    ['brightgreen', 'brightgreen', 'yellowgreen', 'yellowgreen', 'yellow', 'yellow', 'red', 'red'],
  );
});
