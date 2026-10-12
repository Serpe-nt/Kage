/**
 * What the compose end-to-end test of crawls runs (tests/e2e/crawl.sh), with
 * a client that was generated from the API's document and is not Rust:
 *
 * - the reading room of the fixture site (`/site/`) crawled by a persona of
 *   each engine: the pages inside the depth and no others, each with its
 *   depth, how it was reached, what it repeats, and how its scrolling ended;
 * - a page that is left by a click and by a click after going back, for
 *   the fixture's own account of what it saw;
 * - a crawl with sitemaps, which a scout reads and never the site's reader;
 * - a crawl under a pacing profile with a rest between two pages;
 * - a crawl that is cancelled while it reads.
 *
 *   KAGE_API_URL   where the API listens
 *   KAGE_API_KEY   a key with the `jobs` and `crawl` scopes
 *   KAGE_E2E_SITE  the fixture site, for example http://fixture.kage.test:8088
 *
 * It prints one JSON document and exits 0 when everything held.
 */
import assert from 'node:assert/strict';

import {
  cancelJob,
  crawlProgress,
  createCrawl,
  createKageClient,
  KageProblemError,
  readCrawlPages,
  readJob,
  waitForJob,
  type CrawledPage,
  type CrawlRequest,
  type KageClient,
} from './client.js';

function env(name: string): string {
  const value = process.env[name];
  if (value === undefined || value === '') {
    throw new Error(`${name} is not set`);
  }
  return value;
}

const ENGINES = ['chromium', 'firefox'] as const;
type EngineName = (typeof ENGINES)[number];

/** What every page of these crawls is fetched with: read fast, and read. */
const READ = { behavior: { scroll_depth: 0.9, dwell_ms: 1000 }, timeout_ms: 30_000 };

/**
 * A page of the fixture by a name that does not change with how its link
 * was spelled: the path, with the host in front when it is not the site's
 * first one. Campaign tags and the fragment are no part of it.
 */
function nameOf(address: string, site: string): string {
  const url = new URL(address);
  for (const name of [...url.searchParams.keys()]) {
    if (name.startsWith('utm_')) {
      url.searchParams.delete(name);
    }
  }
  const host = url.host === new URL(site).host ? '' : `${url.hostname.split('.')[0] ?? ''}:`;
  return `${host}${url.pathname}${url.search}`;
}

/** The reading room inside three links of its home page, by depth. */
const READING_ROOM: Readonly<Record<string, number>> = {
  '/site/': 0,
  '/site/articles': 1,
  '/site/a': 1,
  '/site/about': 1,
  '/site/tall': 1,
  '/site/b': 2,
  '/site/c': 2,
  '/site/dup-1': 2,
  '/site/dup-2': 2,
  '/site/near-1': 2,
  '/site/near-2': 2,
  '/site/deep/1': 2,
  '/site/moved': 2,
  '/site/missing': 2,
  '/site/a/notes': 2,
  '/site/private': 2,
  'lenient:/site/annex': 2,
  '/site/deep/2': 3,
  'lenient:/site/': 3,
  'lenient:/site/articles': 3,
  'lenient:/site/about': 3,
};

/** The one persona that read a crawl, by its handle, and its engine. */
function readerOf(pages: readonly CrawledPage[]) {
  const readers = new Map<string, string>();
  for (const { item } of pages) {
    const persona = item.result?.persona;
    if (persona !== undefined && persona !== null) {
      readers.set(persona.id, persona.engine);
    }
  }
  assert.equal(readers.size, 1, `one visitor reads a site: ${JSON.stringify([...readers])}`);
  const [reader] = [...readers];
  assert.ok(reader !== undefined);
  return { id: reader[0], engine: reader[1] };
}

function count<T>(values: readonly T[]): Record<string, number> {
  const counts: Record<string, number> = {};
  for (const value of values) {
    const key = String(value);
    counts[key] = (counts[key] ?? 0) + 1;
  }
  return counts;
}

async function run(client: KageClient, request: CrawlRequest, timeoutMs: number) {
  const started = Date.now();
  const { job } = await createCrawl(client, request);
  assert.equal(job.type, 'crawl');
  // A crawl has no items when it is accepted.
  assert.equal(job.items.total, 0);
  const done = await waitForJob(client, job.id, { timeoutMs, everyMs: 2000 });
  const pages = await readCrawlPages(client, job.id, { pageSize: 200 });
  return { done, pages, seconds: Math.round((Date.now() - started) / 1000) };
}

/** The reading room, read by a persona of one engine. */
async function readingRoom(client: KageClient, site: string, engine: EngineName) {
  const { done, pages, seconds } = await run(
    client,
    {
      seeds: [`${site}/site/`],
      max_depth: 3,
      defaults: { ...READ, include: ['headers'], persona: { engines: [engine] } },
    },
    900_000,
  );
  const crawl = crawlProgress(done);
  assert.equal(done.state, 'succeeded', JSON.stringify(done));
  assert.equal(crawl.ended, 'completed');

  // The pages inside the depth and no others, each at its depth.
  const depths = Object.fromEntries(pages.map((page) => [nameOf(page.url, site), page.page.depth]));
  assert.deepEqual(
    Object.fromEntries(Object.entries(depths).sort()),
    Object.fromEntries(Object.entries(READING_ROOM).sort()),
  );
  assert.equal(pages.length, Object.keys(READING_ROOM).length, 'a page was asked for twice');
  assert.equal(crawl.pages, pages.length);
  assert.equal(done.items.total, pages.length);
  assert.equal(done.items.succeeded, pages.length);
  const by = new Map(pages.map((page) => [nameOf(page.url, site), page]));
  const page = (name: string) => {
    const found = by.get(name);
    assert.ok(found !== undefined, `${name} was not read`);
    return found;
  };

  // One persona of the engine asked for read it, in a browser.
  const reader = readerOf(pages);
  assert.equal(reader.engine, engine);
  for (const { item } of pages) {
    assert.equal(item.result?.attempts.at(-1)?.render, 'browser');
  }

  // How the pages were reached: the seed typed, and from then on by a
  // click on a link of the open page, or of a page the persona went back
  // to; all three in every engine.
  assert.equal(page('/site/').page.reached, 'typed');
  assert.equal(page('/site/').page.found_by, 'seed');
  const reached = count(pages.map((one) => one.page.reached));
  assert.ok((reached['click'] ?? 0) >= 4, JSON.stringify(reached));
  assert.ok((reached['back_click'] ?? 0) >= 1, JSON.stringify(reached));
  assert.equal(reached['referer'] ?? 0, 0);
  for (const one of pages) {
    const backs = one.page.backs ?? null;
    if (one.page.reached === 'click') assert.equal(backs, 0);
    if (one.page.reached === 'back_click') assert.ok(backs !== null && backs >= 1 && backs <= 3);
    if (one.page.reached === 'typed') assert.equal(backs, null);
  }
  assert.deepEqual(crawl.reached, {
    typed: reached['typed'] ?? 0,
    click: reached['click'] ?? 0,
    back_click: reached['back_click'] ?? 0,
    referer: 0,
  });
  // A page that a click led to was asked for under the address the page
  // it was clicked on gives: the home page names the first article with a
  // campaign tag, and that is where the crawl first met it; the listing,
  // where it is clicked, names it without.
  const first = page('/site/a');
  assert.ok(first.page.found_on?.endsWith('/site/'), JSON.stringify(first.page));
  assert.equal(first.page.reached, 'click');
  assert.equal(first.url, `${site}/site/a`);

  // What nobody can see was not followed: five links of the home page,
  // two the markup hides and three only a browser knows of.
  assert.equal(crawl.skipped['hidden'], 5, JSON.stringify(crawl.skipped));
  assert.ok(!Object.keys(depths).some((name) => name.includes('/site/trap')));
  assert.equal(crawl.skipped['off_site'], 1);
  assert.equal(crawl.skipped['not_a_page'], 1);
  assert.equal(crawl.skipped['too_deep'], 1);

  // What repeats is flagged: one text under two addresses, an address
  // that moved to a page read before, the pages of the other host that
  // say what the first host's say; and one text with a sentence changed.
  const second = page('/site/dup-2').page.duplicate_of ?? page('/site/dup-1').page.duplicate_of;
  assert.ok(second?.includes('/site/dup-'), 'the copy is not flagged');
  assert.ok(page('/site/moved').page.duplicate_of?.includes('/site/a'));
  assert.ok(page('/site/moved').page.final_url?.endsWith('/site/a'));
  for (const name of ['lenient:/site/', 'lenient:/site/articles', 'lenient:/site/about']) {
    assert.ok(typeof page(name).page.duplicate_of === 'string', `${name} is not flagged`);
  }
  assert.equal(crawl.duplicates, 5);
  const near =
    page('/site/near-2').page.near_duplicate_of ?? page('/site/near-1').page.near_duplicate_of;
  assert.ok(near !== undefined && near !== null, 'the near-copy is not flagged');
  assert.ok(near.url.includes('/site/near-') && near.bits <= 6, JSON.stringify(near));
  assert.equal(crawl.near_duplicates, 1);
  // A page the site has no more is a fetch that was made and no page of
  // the crawl.
  assert.equal(page('/site/missing').item.result?.status, 404);
  assert.equal(crawl.failed, 1);
  assert.equal(crawl.fetched, pages.length - 1);

  // The long page was left when the persona's patience with it was used
  // up, and says how far down it came; a short one was read to where the
  // persona meant to go, or to its end.
  const tall = page('/site/tall').page.scroll;
  assert.ok(tall !== undefined && tall !== null);
  assert.equal(tall.ended_by, 'patience', JSON.stringify(tall));
  assert.ok(typeof tall.reached === 'number' && tall.reached < 0.6, JSON.stringify(tall));
  assert.ok(typeof tall.patience_ms === 'number' && tall.patience_ms >= 2000);
  const patiences = new Set(pages.map((one) => one.page.scroll?.patience_ms));
  assert.ok(patiences.size >= pages.length - 2, 'pages share a patience');
  const ends = count(pages.map((one) => one.page.scroll?.ended_by));

  return {
    id: done.id,
    engine,
    persona: reader.id,
    pages: pages.length,
    seconds,
    reached,
    order: pages.map((one) => `${nameOf(one.url, site)} ${one.page.reached ?? '-'}`),
    skipped: crawl.skipped,
    duplicates: crawl.duplicates,
    near_duplicate_bits: near.bits,
    tall,
    scroll_ends: ends,
    // For the site's own word on the rest between two pages.
    times: pages.map((one) => ({
      url: one.url,
      started_at: one.item.started_at,
      finished_at: one.item.finished_at,
    })),
  };
}

/** A page that tells the site how it was left, left twice by a click. */
async function leaving(client: KageClient, site: string, engine: EngineName) {
  const marker = `crawl-${engine}`;
  const { done, pages } = await run(
    client,
    {
      seeds: [`${site}/leave/?run=${marker}`],
      max_depth: 1,
      defaults: { ...READ, persona: { engines: [engine] } },
    },
    300_000,
  );
  assert.equal(done.state, 'succeeded', JSON.stringify(done));
  assert.deepEqual(
    pages.map((page) => [nameOf(page.url, site), page.page.reached, page.page.backs ?? null]),
    [
      [`/leave/?run=${marker}`, 'typed', null],
      // The link in view is clicked on the open page; for the one further
      // down the persona goes back to the page first.
      ['/landing?from=leave', 'click', 0],
      ['/landing?from=below', 'back_click', 1],
    ],
  );
  // Two pages that say the same.
  assert.ok(pages[2]?.page.duplicate_of?.includes('/landing?from=leave'));
  return { engine, marker, persona: readerOf(pages).id };
}

/** A crawl that starts from what the site's sitemaps name. */
async function withSitemaps(client: KageClient, site: string, engine: EngineName) {
  const { done, pages } = await run(
    client,
    {
      seeds: [`${site}/site/`],
      use_sitemaps: true,
      // The seeds and what the sitemaps name, and nothing they link to.
      max_depth: 0,
      defaults: { ...READ, persona: { engines: [engine] } },
    },
    600_000,
  );
  const crawl = crawlProgress(done);
  assert.equal(done.state, 'succeeded', JSON.stringify(done));
  const origin = new URL(site).origin;
  const state = (path: string) => crawl.sitemaps.find((one) => one.url === `${origin}${path}`);
  assert.equal(crawl.sitemaps.length, 3, JSON.stringify(crawl.sitemaps));
  assert.deepEqual(state('/sitemap.xml'), {
    site: 'kage.test',
    url: `${origin}/sitemap.xml`,
    state: 'index',
    urls: 2,
  });
  assert.equal(state('/sitemaps/pages.xml')?.state, 'read');
  assert.equal(state('/sitemaps/pages.xml')?.urls, 3);
  const packed = state('/sitemaps/more.xml.gz');
  const read = pages.map((page) => nameOf(page.url, site)).sort();
  const reader = readerOf(pages);
  assert.equal(reader.engine, engine);
  if (packed?.state === 'read') {
    // A Firefox scout read the gzipped file: the pages only it names are
    // read, at depth 0, with what it says about them.
    assert.deepEqual(read, [
      '/site/',
      '/site/a',
      '/site/b',
      '/site/c',
      '/site/only-1',
      '/site/only-2',
    ]);
    const only = pages.find((page) => page.url.endsWith('/site/only-1'));
    assert.equal(only?.page.found_by, 'sitemap');
    assert.equal(only?.page.depth, 0);
    assert.equal(only?.page.lastmod, '2026-10-05T08:30:00Z');
  } else {
    // A Chrome scout was shown nothing of it.
    assert.equal(packed?.state, 'sitemap_unreadable', JSON.stringify(packed));
    assert.deepEqual(read, ['/site/', '/site/a', '/site/b']);
  }
  assert.ok(pages.every((page) => page.page.depth === 0));
  return {
    engine,
    reader: reader.id,
    sitemaps: crawl.sitemaps,
    pages: read,
    // For the site's own word on who asked for what: the sitemaps were
    // asked for before the first page, the pages from then on.
    created_at: done.created_at,
    first_page_at: pages[0]?.item.started_at,
    finished_at: done.finished_at,
  };
}

/** A chain of pages under a pacing profile that rests between two pages. */
async function rested(client: KageClient, site: string) {
  const { done, pages } = await run(
    client,
    {
      seeds: [`${site}/site/deep/1`],
      max_depth: 5,
      max_pages: 6,
      deny: ['/site/(articles|about)?$'],
      defaults: {
        ...READ,
        pacing: { profile: 'e2e_rested' },
        persona: { engines: ['chromium'] },
      },
    },
    300_000,
  );
  assert.equal(done.state, 'succeeded', JSON.stringify(done));
  assert.deepEqual(
    pages.map((page) => nameOf(page.url, site)),
    [1, 2, 3, 4, 5, 6].map((part) => `/site/deep/${part}`),
  );
  assert.deepEqual(
    pages.map((page) => page.page.reached),
    ['typed', 'click', 'click', 'click', 'click', 'click'],
  );
  return {
    times: pages.map((one) => ({
      url: one.url,
      started_at: one.item.started_at,
      finished_at: one.item.finished_at,
    })),
  };
}

/** A crawl of slow pages, cancelled while it reads. */
async function cancelled(client: KageClient, site: string) {
  const { job } = await createCrawl(client, {
    seeds: [`${site}/site/slow/1?ms=1500`],
    max_depth: 32,
    max_pages: 30,
    deny: ['/site/(articles|about)?$'],
    defaults: { behavior: { scroll_depth: 0, dwell_ms: 600 }, timeout_ms: 30_000 },
  });
  await waitForJob(client, job.id, {
    timeoutMs: 180_000,
    everyMs: 500,
    done: (seen) => seen.items.succeeded >= 2,
  });
  const atCancel = await cancelJob(client, job.id);
  assert.equal(atCancel.cancel_requested, true);
  const done = await waitForJob(client, job.id, { timeoutMs: 120_000, everyMs: 500 });
  assert.equal(done.state, 'cancelled', JSON.stringify(done));
  const crawl = crawlProgress(done);
  assert.equal(crawl.ended, 'cancelled');
  // The page that was being read is finished, and not one more is asked for.
  assert.ok(
    done.items.succeeded <= atCancel.items.succeeded + 1,
    `${done.items.succeeded} pages ended, ${atCancel.items.succeeded} had when the cancel came`,
  );
  assert.equal(done.items.pending, 0);
  assert.equal(crawl.fetching, 0);
  assert.ok(crawl.waiting >= 1, 'nothing was left to read');
  // Cancelling again changes nothing.
  assert.equal((await cancelJob(client, job.id)).state, 'cancelled');
  assert.equal((await readJob(client, job.id)).items.succeeded, done.items.succeeded);
  return {
    id: job.id,
    succeeded_at_cancel: atCancel.items.succeeded,
    succeeded: done.items.succeeded,
    cancelled: done.items.cancelled,
    asked: crawl.pages,
  };
}

/** A crawl that cannot be accepted is refused whole, and says what about. */
async function refused(client: KageClient, site: string) {
  const codes: string[] = [];
  for (const request of [
    { seeds: [] },
    { seeds: [`${site}/site/`, 'http://169.254.169.254/latest/meta-data'] },
    { seeds: [`${site}/site/`], allow: ['('] },
    { seeds: [`${site}/site/`, `${site}/site/private`], deny: ['/private'] },
  ] satisfies CrawlRequest[]) {
    const problem = await createCrawl(client, request).then(
      () => null,
      (error: unknown) => error,
    );
    assert.ok(problem instanceof KageProblemError, `accepted: ${JSON.stringify(request)}`);
    assert.equal(problem.status, 422);
    codes.push(problem.problem.code);
  }
  return codes;
}

async function main() {
  const site = env('KAGE_E2E_SITE');
  const client = createKageClient({ baseUrl: env('KAGE_API_URL'), token: env('KAGE_API_KEY') });

  const refusals = await refused(client, site);
  const rooms = [];
  const left = [];
  const sitemaps = [];
  // One engine after the other: each crawl has the site to itself, so the
  // site's own log says which request was whose.
  for (const engine of ENGINES) {
    rooms.push(await readingRoom(client, site, engine));
    left.push(await leaving(client, site, engine));
    sitemaps.push(await withSitemaps(client, site, engine));
  }
  // Two personas of two engines read the same site, each on its own.
  assert.notEqual(rooms[0]?.persona, rooms[1]?.persona);
  const rest = await rested(client, site);
  const cancel = await cancelled(client, site);

  process.stdout.write(
    `${JSON.stringify({ refusals, rooms, left, sitemaps, rested: rest, cancelled: cancel })}\n`,
  );
}

main().catch((error: unknown) => {
  process.stderr.write(
    `${error instanceof Error ? (error.stack ?? error.message) : String(error)}\n`,
  );
  process.exitCode = 1;
});
