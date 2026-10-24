// The field campaign runner's pure part (node --test, in `just test-scripts`).
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

import {
  afterBlock,
  attemptsFromMetrics,
  cookieNames,
  COMMON_DENY,
  crawlRequest,
  dealPersonas,
  domainsFile,
  hostMatches,
  inWindow,
  jsRegex,
  leaks,
  localTime,
  median,
  pagesAllowed,
  parseSites,
  policyDifferences,
  policyEntry,
  renderSection,
  scrubPrivate,
  sessionIds,
  sessionRequest,
  shuffle,
  siteOf,
  SitesError,
  stopReason,
  vendorHints,
  windowEnd,
} from './lib.mjs';

const PERSONA_A = '01900000-0000-7000-8000-00000000000a';
const PERSONA_B = '01900000-0000-7000-8000-00000000000b';
const PERSONA_C = '01900000-0000-7000-8000-00000000000c';

const round = (sites, extra = {}) =>
  JSON.stringify({
    round: 'm13-test',
    draw_seed: 'a seed for the tests',
    since: '2026-10-24T00:00:00Z',
    sites,
    ...extra,
  });

const SHOP = {
  index: '7',
  domain: 'shop.example',
  seed: 'https://www.shop.example/',
  category: 'retail',
  country: 'FR',
  engine: 'firefox',
  persona: PERSONA_A,
  timezone: 'Europe/Paris',
  names: ['shopbrand'],
};
const NEWS = {
  index: '8',
  domain: 'news.example',
  seed: 'https://news.example/front',
  category: 'news',
  country: 'FR',
  engine: null,
  persona: null,
  timezone: 'Europe/Paris',
};
const CONTROL = {
  index: 'C1',
  domain: 'control.example',
  seed: 'https://www.control.example/',
  category: 'control, no defence',
  country: 'FR',
  timezone: 'Europe/Paris',
  control: true,
  pages: 3,
};

test('a sites file is read with its defaults, and every wrong field is named', () => {
  const parsed = parseSites(round([SHOP, NEWS, CONTROL]));
  assert.equal(parsed.budget.pages_per_day, 12);
  assert.equal(parsed.stops.max_round_bytes, 3_000_000_000);
  assert.deepEqual(parsed.deny, COMMON_DENY);
  const shop = siteOf(parsed, 7);
  assert.equal(shop.pages, 10);
  assert.equal(shop.max_depth, 2);
  assert.deepEqual(shop.window, { from: '09:30', until: '20:00' });
  assert.equal(shop.entry.robots, 'enforce');
  assert.equal(siteOf(parsed, 'C1').entry.robots, 'report');
  assert.throws(() => siteOf(parsed, 9), SitesError);

  const wrong = (site, pattern, extra) => assert.throws(() => parseSites(round([site], extra)), pattern);
  wrong({ ...SHOP, domain: 'Shop.Example' }, /sites\[0\]\.domain/);
  wrong({ ...SHOP, seed: 'https://other.example/' }, /sites\[0\]\.seed/);
  wrong({ ...SHOP, seed: 'ftp://www.shop.example/' }, /sites\[0\]\.seed/);
  wrong({ ...SHOP, country: 'fr' }, /country/);
  wrong({ ...SHOP, engine: 'webkit' }, /engine/);
  wrong({ ...SHOP, persona: 'p_0123456789abcdef' }, /persona/);
  wrong({ ...SHOP, timezone: 'Mars/Olympus' }, /timezone/);
  wrong({ ...SHOP, window: { from: '20:00', until: '09:30' } }, /window/);
  wrong({ ...SHOP, pages: 13 }, /pages/);
  wrong({ ...SHOP, deny: ['(unclosed'] }, /deny\[0\]/);
  wrong({ ...SHOP, entry: { min_level: 2 } }, /min_level/);
  wrong({ ...SHOP, entry: { robots: 'obey' } }, /robots/);
  wrong(SHOP, /pages_per_day/, { budget: { pages_per_day: 20 } });
  assert.throws(() => parseSites(round([SHOP, { ...NEWS, persona: PERSONA_A }])), /one persona reads one site/);
  assert.throws(() => parseSites(round([SHOP, { ...SHOP }])), /index.*twice/);
  assert.throws(() => parseSites('{'), /no JSON/);
  // A control may be read by a persona that reads a site of the round.
  assert.doesNotThrow(() => parseSites(round([SHOP, { ...CONTROL, persona: PERSONA_A }])));
});

test('the example sites file is a valid round', () => {
  const example = parseSites(readFileSync(new URL('./sites.example.json', import.meta.url), 'utf8'));
  assert.equal(example.sites.length, 2);
  assert.equal(siteOf(example, 'C1').control, true);
  assert.deepEqual(leaks('## Site 1: DE retail, none seen; key "site-1.example"', example), []);
  assert.deepEqual(leaks('read https://example.org/', example), ['the name "example.org"', 'an address of a page']);
});

test('the deny patterns keep a reader off the pages where people sign in, buy or search', () => {
  const denied = (url) => COMMON_DENY.some((pattern) => jsRegex(pattern).test(url));
  for (const url of [
    'https://www.shop.example/login',
    'https://www.shop.example/de/Anmelden?return=/',
    'https://www.shop.example/mon-compte/commandes',
    'https://www.shop.example/warenkorb',
    'https://www.shop.example/checkout/step-1',
    'https://www.shop.example/suche?q=jacke',
    'https://www.shop.example/catalogue?query=shoes',
    'https://www.shop.example/carrello',
    'https://www.shop.example/wishlist',
  ]) {
    assert.ok(denied(url), url);
  }
  for (const url of [
    'https://www.shop.example/',
    'https://www.shop.example/damen/jacken/',
    'https://www.shop.example/product/blue-coat-42.html',
    'https://www.shop.example/accounting-software-review',
    'https://www.shop.example/carts-and-trolleys/',
  ]) {
    assert.ok(!denied(url), url);
  }
});

test('hosts belong to a site by its registrable domain', () => {
  assert.ok(hostMatches('www.shop.example', 'shop.example'));
  assert.ok(hostMatches('shop.example', 'shop.example'));
  assert.ok(hostMatches('WWW2.Shop.Example.', 'shop.example'));
  assert.ok(!hostMatches('notshop.example', 'shop.example'));
  assert.ok(!hostMatches('shop.example.evil', 'shop.example'));
});

test('the window is the site\'s own daytime, across the change of the clocks', () => {
  const site = siteOf(parseSites(round([SHOP])), 7);
  // 25 October 2026: Paris goes from UTC+2 to UTC+1 at 01:00 UTC.
  assert.deepEqual(localTime(new Date('2026-10-24T07:00:00Z'), 'Europe/Paris'), { date: '2026-10-24', clock: '09:00' });
  assert.deepEqual(localTime(new Date('2026-10-25T08:00:00Z'), 'Europe/Paris'), { date: '2026-10-25', clock: '09:00' });
  assert.ok(!inWindow(site, new Date('2026-10-25T08:00:00Z')));
  assert.ok(inWindow(site, new Date('2026-10-25T08:30:00Z')));
  assert.ok(!inWindow(site, new Date('2026-10-25T19:00:00Z')));
  assert.equal(windowEnd(site, new Date('2026-10-25T12:00:00Z')).toISOString(), '2026-10-25T19:00:00.000Z');
  assert.equal(windowEnd(site, new Date('2026-10-24T12:00:00Z')).toISOString(), '2026-10-24T18:00:00.000Z');
});

test('the budget gives a site one batch a day, its days, its pages and the round\'s', () => {
  const site = siteOf(parseSites(round([SHOP])), 7);
  const budget = parseSites(round([SHOP])).budget;
  const counts = { today: 0, site: 0, days: 0, round: 0 };
  assert.deepEqual(pagesAllowed(site, budget, counts), { pages: 10, reason: null });
  assert.match(pagesAllowed(site, budget, { ...counts, today: 3, days: 1 }).reason, /batch today/);
  assert.match(pagesAllowed(site, budget, { ...counts, days: 3, site: 30 }).reason, /3 days/);
  assert.deepEqual(pagesAllowed(site, budget, { ...counts, days: 2, site: 55 }), { pages: 5, reason: null });
  assert.match(pagesAllowed(site, budget, { ...counts, days: 2, site: 60 }).reason, /60 pages/);
  assert.deepEqual(pagesAllowed(site, budget, { ...counts, round: 596 }), { pages: 4, reason: null });
  assert.match(pagesAllowed(site, budget, { ...counts, round: 600 }).reason, /round/);
});

test('the round stops for a leak, a thin pool, the plan\'s stop, the term\'s share and its own bytes', () => {
  const stops = parseSites(round([SHOP])).stops;
  const pool = (proxies, bandwidth) => ({ proxies: { usable: 20, egress_unsafe: false, ...proxies }, bandwidth });
  assert.equal(stopReason(pool({}, { share: 0.1, stop: false }), 10, stops), null);
  assert.match(stopReason(pool({ egress_unsafe: true }, null), 0, stops), /leak/);
  assert.match(stopReason(pool({ usable: 15 }, null), 0, stops), /15 proxies/);
  assert.match(stopReason(pool({}, { share: 0.1, stop: true }), 0, stops), /stop is in force/);
  assert.match(stopReason(pool({}, { share: 0.5 }), 0, stops), /50\.0 %/);
  assert.match(stopReason(pool({}, null), 3_000_000_000, stops), /3000\.0 MB/);
});

test('a country\'s sites are dealt to its personas by a draw that replays', () => {
  const parsed = parseSites(round([{ ...SHOP, persona: null, engine: null }, NEWS, CONTROL]));
  const personas = [
    { id: PERSONA_C, country: 'FR', engine: 'chromium', claimed_os: 'linux', state: 'active' },
    { id: PERSONA_A, country: 'FR', engine: 'firefox', claimed_os: 'windows', state: 'active' },
    { id: PERSONA_B, country: 'FR', engine: 'firefox', claimed_os: 'macos', state: 'retired' },
    { id: '01900000-0000-7000-8000-00000000000d', country: 'DE', engine: 'chromium', claimed_os: 'linux', state: 'active' },
  ];
  const dealt = dealPersonas(parsed, personas);
  assert.equal(dealt.length, 2);
  assert.deepEqual(new Set(dealt.map((one) => one.persona)), new Set([PERSONA_A, PERSONA_C]));
  assert.deepEqual(dealPersonas(parsed, personas.slice().reverse()), dealt);
  for (const one of dealt) assert.equal(one.engine, personas.find((p) => p.id === one.persona).engine);
  assert.throws(() => dealPersonas(parsed, personas.slice(0, 1)), /2 sites and 1 live personas/);
  assert.deepEqual(shuffle([1, 2, 3, 4, 5], 'x'), shuffle([1, 2, 3, 4, 5], 'x'));
  assert.notDeepEqual(shuffle([1, 2, 3, 4, 5, 6, 7, 8], 'x'), shuffle([1, 2, 3, 4, 5, 6, 7, 8], 'y'));
});

test('the policy entries are written under the shipped mapping, and the preview is compared field by field', () => {
  const parsed = parseSites(round([SHOP, CONTROL]));
  const shop = siteOf(parsed, 7);
  const entry = policyEntry(shop);
  assert.match(entry, /^ {2}"shop\.example": # site 7\n/);
  assert.match(entry, /countries: \[FR\]\n {4}engines: \[firefox\]/);
  assert.match(entry, /robots: enforce/);
  assert.match(entry, /active_hours: enforce/);
  assert.doesNotMatch(policyEntry(siteOf(parsed, 'C1')), /countries/);
  assert.match(policyEntry(shop, { key: 'site-7.example' }), /"site-7\.example"/);
  assert.throws(() => policyEntry({ ...shop, engine: null }), /draw the personas first/);

  const shipped = 'defaults:\n  min_level: 0\ndomains:\n  bot.sannysoft.com: { min_level: 3 }\n';
  const file = domainsFile(shipped, parsed);
  assert.ok(file.startsWith(shipped));
  assert.match(file, /"control\.example": # site C1/);
  assert.equal((file.match(/^domains:/gm) ?? []).length, 1);
  assert.throws(() => domainsFile('domains:\n  a: {}\ndefaults: {}\n', parsed), /does not end/);
  assert.match(domainsFile('defaults: {}\ndomains: {}\n', parsed), /^domains:\n/m);

  const view = {
    policy: {
      min_level: 3,
      countries: ['FR'],
      engines: ['firefox'],
      pacing: 'human',
      pacing_locked: true,
      behavior_locked: true,
      robots: 'enforce',
      active_hours: 'enforce',
      retries: 2,
      breaker_threshold: 5,
    },
    min_level: 3,
  };
  assert.deepEqual(policyDifferences(shop, view), []);
  assert.deepEqual(policyDifferences(shop, { ...view, policy: { ...view.policy, robots: 'report', engines: ['chromium'] } }), [
    ['robots', 'enforce', 'report'],
    ['engines', ['firefox'], ['chromium']],
  ]);
});

test('a batch is one crawl pinned to its session, at level 3, keeping headers and extraction metadata only', () => {
  const parsed = parseSites(round([{ ...SHOP, deny: ['(?i)/outlet/'] }]));
  const shop = siteOf(parsed, 7);
  const ids = sessionIds(parsed, shop, '2026-10-25');
  assert.equal(ids.length, 24);
  assert.equal(new Set(ids).size, 24);
  assert.equal(ids[0], 'm13-test-s7-20261025-0');
  assert.ok(ids.every((id) => /^[A-Za-z0-9_.-]{1,64}$/.test(id)));
  assert.deepEqual(sessionRequest(shop, ids[0]), {
    id: ids[0],
    domain: 'www.shop.example',
    persona: { countries: ['FR'], engines: ['firefox'] },
  });
  assert.throws(() => sessionRequest({ ...shop, engine: null }, ids[0]), /no engine for its session/);
  const crawl = crawlRequest(parsed, shop, { sessionId: ids[0], pages: 9 });
  assert.equal(crawl.type, 'crawl');
  assert.deepEqual(crawl.seeds, ['https://www.shop.example/']);
  assert.equal(crawl.max_pages, 9);
  assert.equal(crawl.max_depth, 2);
  assert.equal(crawl.use_sitemaps, false);
  assert.equal(crawl.session_id, ids[0]);
  assert.equal(crawl.deny.at(-1), '(?i)/outlet/');
  assert.deepEqual(crawl.defaults, {
    level: 3,
    include: ['headers', 'extract'],
    extract: { markdown: false, links: false },
  });
  assert.deepEqual(crawlRequest(parsed, shop, { sessionId: 'x', pages: 1, include: ['headers', 'dom'] }).defaults, {
    level: 3,
    include: ['headers', 'dom'],
  });
});

test('a block that cools the persona past the day ends the batch; a short one does not', () => {
  const now = new Date('2026-10-25T10:00:00Z');
  const end = new Date('2026-10-25T19:00:00Z');
  const hard = afterBlock({ outcome: 'challenge_failed', block: { vendor: 'datadome', confidence: 0.95 } }, now, end);
  assert.equal(hard.cooldown_ms, Math.round(24 * 3_600_000 * 0.95));
  assert.ok(hard.ends_day);
  const rate = afterBlock({ outcome: 'rate_limited', block: { vendor: 'cloudflare', confidence: 0.9 } }, now, end);
  assert.ok(!rate.ends_day);
  const weak = afterBlock({ outcome: 'forbidden', block: { vendor: 'generic', confidence: 0.6 } }, now, end);
  assert.equal(weak.cooldown_ms, 18 * 60_000);
  assert.ok(!weak.ends_day);
  const late = afterBlock({ outcome: 'soft_block', block: { vendor: 'generic', confidence: 0.6 } }, new Date('2026-10-25T18:58:00Z'), end);
  assert.equal(late.cooldown_ms, 18 * 60_000);
  assert.ok(late.ends_day);
  assert.equal(afterBlock({ outcome: 'forbidden', block: { vendor: 'generic', confidence: 0.1 } }, now, end).cooldown_ms, 5 * 60_000);
});

test('vendors are read from header names, the server and the names of cookies, never their values', () => {
  const headers = [
    { name: 'Server', value: 'AkamaiGHost' },
    { name: 'set-cookie', value: '_abck=[redacted]; Path=/; Secure' },
    { name: 'set-cookie', value: 'datadome=[redacted]; Max-Age=31536000' },
    { name: 'Set-Cookie', value: 'QueueITAccepted-SDFrts345E-V3_x=[redacted]' },
    { name: 'cf-ray', value: '8f00000000000000-FRA' },
    { name: 'set-cookie', value: 'session=[redacted]' },
  ];
  assert.deepEqual(vendorHints(headers), ['akamai', 'cloudflare', 'datadome', 'queue_it']);
  assert.deepEqual(cookieNames(headers), ['QueueITAccepted-SDFrts345E-V3_x', '_abck', 'datadome', 'session']);
  assert.deepEqual(vendorHints([]), []);
  assert.deepEqual(vendorHints(undefined), []);
});

test('the attempts of a site\'s hosts are summed from /metrics', () => {
  const text = [
    '# TYPE kage_attempts_total counter',
    'kage_attempts_total{domain="www.shop.example",level="3",outcome="ok",vendor="akamai"} 7',
    'kage_attempts_total{domain="en.shop.example",level="3",outcome="ok",vendor="akamai"} 2',
    'kage_attempts_total{domain="www.shop.example",level="3",outcome="challenge_failed",vendor="akamai"} 1',
    'kage_attempts_total{domain="notshop.example",level="3",outcome="ok",vendor=""} 5',
    'kage_requests_total{domain="www.shop.example",level="3",outcome="ok",vendor="akamai"} 9',
  ].join('\n');
  assert.deepEqual(attemptsFromMetrics(text, 'shop.example'), [
    { level: '3', outcome: 'ok', vendor: 'akamai', count: 9 },
    { level: '3', outcome: 'challenge_failed', vendor: 'akamai', count: 1 },
  ]);
});

test('private evidence loses addresses, e-mail addresses, phone numbers and tokens, and keeps the rest', () => {
  const scrubbed = scrubPrivate({
    url: 'https://www.shop.example/p/42?token=abc123&color=blue',
    note: 'from 203.0.113.9 and 2001:db8::1, write to jane.doe@example.org or +33 6 12 34 56 78',
    list: ['10.0.0.1', 42, null],
    when: '2026-10-25T10:00:00Z',
    reference: 'Reference #18.2d351ab8.1760000000.a4e16ab',
  });
  assert.equal(scrubbed.url, 'https://www.shop.example/p/42?token=[token]&color=blue');
  assert.equal(scrubbed.note, 'from [address] and [address], write to [email] or [phone]');
  assert.deepEqual(scrubbed.list, ['[address]', 42, null]);
  assert.equal(scrubbed.when, '2026-10-25T10:00:00Z');
  assert.equal(scrubbed.reference, 'Reference #18.2d351ab8.1760000000.a4e16ab');
});

test('a committed text may name no site, no address, no id and no token', () => {
  const parsed = parseSites(round([SHOP, NEWS]));
  assert.deepEqual(leaks('## Site 7: FR retail, akamai\n\n- 10 pages, 9 read; site-7.example', parsed), []);
  const found = (text) => leaks(text, parsed);
  assert.deepEqual(found('the shop.example home page'), ['the name "shop.example"', 'the name "shop"']);
  assert.deepEqual(found('a ShopBrand banner'), ['the name "shopbrand"']);
  assert.deepEqual(found('the news front page'), ['the name "news"']);
  assert.deepEqual(found('a newsletter'), []);
  assert.deepEqual(found('exit 198.51.100.7'), ['an IPv4 address']);
  assert.deepEqual(found('exit 2001:db8:0:0:1:2:3:4'), ['an IPv6 address']);
  assert.deepEqual(found('mail ops@corp.example'), ['an e-mail address']);
  assert.deepEqual(found(`persona ${PERSONA_A}`), ["an id (a persona's or a request's)"]);
  assert.deepEqual(found('persona p_0123456789abcdef'), ['a persona handle']);
  assert.deepEqual(found('cookie AbCdEf0123456789AbCdEf0123456789xyz'), ['a long token']);
  assert.deepEqual(found('see https://elsewhere.example/page'), ['an address of a page']);
  assert.deepEqual(leaks('the brand hm', parsed, ['brand']), ['the name "brand"']);
  assert.deepEqual(found('until 2026-10-25T18:25:42.950Z'), ['a time finer than the hour']);
  assert.deepEqual(found('until 2026-10-25T18:00:00Z, on 2026-10-25'), []);
});

test('a site\'s section says what was read, how, and what blocked it, and leaks nothing', () => {
  const parsed = parseSites(round([SHOP]));
  const shop = siteOf(parsed, 7);
  const page = (outcome, extra = {}) => ({
    outcome,
    reached: 'click',
    scroll_ended_by: 'patience',
    total_ms: 40_000,
    word_count: 300,
    vendors: ['akamai'],
    ...extra,
  });
  const before = {
    date: '2026-10-25',
    persona: { engine: 'firefox', country: 'FR', claimed_os: 'windows' },
    rates: [
      { level: 3, engine: 'firefox', outcome: 'ok', attempts: 3, percent: 75 },
      { level: 3, engine: 'firefox', outcome: 'challenge_failed', attempts: 1, percent: 25 },
    ],
    pages: [page('ok', { reached: 'typed' }), page('ok'), page('ok'), page('challenge_failed', { scroll_ended_by: null, word_count: 12 })],
    blocks: [{ vendor: 'akamai', type: 'bot_manager_challenge' }],
    metrics: [{ level: '3', outcome: 'ok', vendor: 'akamai', count: 3 }],
    skipped: { robots_denied: 2, hidden: 0 },
    robots: 'fetched by the scout, 4 allowed, 0 disallowed',
    ended: 'cancelled after a block that cools the persona past the day',
    pacing: 'human (locked)',
    bytes: 8_000_000,
    decision: 'pass 2: one change, to be approved',
  };
  const section = renderSection(shop, [before]);
  assert.match(section, /^## Site 7: FR retail, akamai\n/);
  assert.match(section, /one Firefox persona in FR claiming Windows/);
  assert.match(section, /blocks: akamai bot_manager_challenge 1/);
  assert.match(section, /\| 3 \| firefox \| challenge_failed \| 1 \| 25\.0 \|/);
  assert.match(section, /4 pages asked, 3 read; reached: click 3, typed 1/);
  assert.match(section, /skipped: robots_denied 2/);
  assert.match(section, /Change: none/);
  assert.match(section, /8\.0 MB \(2\.0 MB a page\)/);
  assert.deepEqual(leaks(section, parsed), []);

  const timed = renderSection(shop, [{ ...before, ended: 'cancelled by the runner: the persona cools until 2026-10-25T18:25:42.950Z' }]);
  assert.match(timed, /cools until 2026-10-25T18:00Z/);
  assert.deepEqual(leaks(timed, parsed), []);
  const quiet = { ...before, blocks: [], pages: before.pages.map((p) => ({ ...p, vendors: [] })), metrics: [{ level: '3', outcome: 'ok', vendor: 'none', count: 4 }] };
  assert.match(renderSection(shop, [quiet]), /^## Site 7: FR retail, none seen\n/);
  assert.match(renderSection({ ...shop, control: true }, [quiet]), /a reader of a site of the round that day/);

  const after = { ...before, date: '2026-10-27', change: 'pacing `patient`', rates: before.rates.slice(0, 1), blocks: [], decision: 'keep' };
  const both = renderSection(shop, [before, after]);
  assert.match(both, /Change: pacing `patient`/);
  assert.match(both, /\*\*After\*\* \(pass 2, 2026-10-27\)/);
  assert.match(both, /Decision: keep/);
});

test('a median is the lower middle and ignores what is no number', () => {
  assert.equal(median([3, 1, 2]), 2);
  assert.equal(median([4, 1, 3, 2]), 2);
  assert.equal(median([null, 5, Number.NaN]), 5);
  assert.equal(median([]), null);
});
