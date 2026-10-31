/**
 * What the compose end-to-end test of extraction, `robots.txt` and the
 * response cache runs (tests/e2e/extract.sh), with a client that was
 * generated from the API's document and is not Rust:
 *
 * - an essay of the fixture site fetched with `include: ["extract"]` by a
 *   persona of each engine: its metadata, its headings, its text and its
 *   markdown;
 * - the same request again, answered from the response cache, and another
 *   include set, which is a visit;
 * - a page whose script nests its text ten thousand elements deep, read by
 *   the Firefox persona at level 2: the result is whole, and the server
 *   stands (F97);
 * - the site under the name whose policy enforces its `robots.txt`: a page
 *   the file allows, a page it disallows, and a request that asks for less
 *   than the policy enforces;
 * - a crawl there with sitemaps: the page the file disallows is never
 *   asked for, and the sitemap the file names is the one that is read.
 *
 *   KAGE_API_URL    where the API listens
 *   KAGE_API_KEY    a key with the `fetch`, `jobs` and `crawl` scopes
 *   KAGE_E2E_SITE   the fixture under the name whose policy leaves its
 *                   robots.txt alone, for example http://fixture.kage.test:8088
 *   KAGE_E2E_RULES  the same site under the name whose policy enforces it
 *
 * It prints one JSON document and exits 0 when everything held. What the
 * site itself saw is the shell's to check.
 */
import assert from 'node:assert/strict';

import {
  cachedAt,
  crawlProgress,
  createCrawl,
  createKageClient,
  extractOf,
  fetchPage,
  isResult,
  KageProblemError,
  readCrawlPages,
  readResult,
  robotsDenial,
  waitForJob,
  type FetchRequest,
  type FetchResult,
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

/**
 * Read fast, by a person (level 3): the test is about what comes back, not
 * about the visit.
 */
const QUICK = {
  level: 3,
  behavior: { scroll_depth: 0, dwell_ms: 1000 },
  timeout_ms: 60_000,
} as const;

const ESSAY = 'The lamplighter and the ledger';

/** A fetch that ended inside the server's wall time. */
async function finished(client: KageClient, request: FetchRequest): Promise<FetchResult> {
  const fetched = await fetchPage(client, request);
  assert.ok(isResult(fetched), `${request.url}: the fetch outlasted the wall time`);
  return fetched;
}

/** The code a request is refused with. */
async function refusal(client: KageClient, request: FetchRequest): Promise<string> {
  const error: unknown = await fetchPage(client, request).then(
    () => null,
    (thrown: unknown) => thrown,
  );
  assert.ok(error instanceof KageProblemError, `${request.url} was not refused`);
  assert.equal(error.status, 422);
  return error.problem.code;
}

/** What is asked of the essay for one engine: an address of its own, so
 * that what the site saw of it is this reader's and nobody else's. */
function essayRequest(site: string, engine: EngineName): FetchRequest {
  return {
    url: `${site}/read/essay?reader=${engine}`,
    include: ['headers', 'extract'],
    persona: { engines: [engine] },
    cache_ttl_s: 600,
    ...QUICK,
  };
}

/** The essay, extracted, and then again from the cache. */
async function essay(client: KageClient, site: string, engine: EngineName) {
  const asking = essayRequest(site, engine);

  const first = await finished(client, asking);
  assert.equal(first.outcome, 'ok', `${engine}: ${first.reason ?? first.outcome}`);
  assert.equal(first.level, 3);
  assert.equal(first.persona?.engine, engine);
  assert.equal(first.cached, false);
  assert.equal(cachedAt(first), undefined);
  // The policy of this name leaves robots.txt alone: nobody asked.
  assert.deepEqual(first.robots, {
    fetched: false,
    allowed: true,
    matched_rule: null,
    crawl_delay: null,
  });

  const said = extractOf(first);
  assert.equal(said.title, `${ESSAY} | The reading room`);
  assert.equal(
    said.description,
    'How a harbour town kept its lamps lit: an essay from the reading room.',
  );
  assert.equal(said.canonical, `${site}/read/essay`);
  assert.equal(said.lang, 'en');
  // A time with a zone is given in UTC.
  assert.equal(said.published_at, '2026-09-28T05:30:00Z');
  assert.equal(said.modified_at, '2026-10-02T09:00:00Z');
  assert.deepEqual(said.opengraph['og:title'], [ESSAY]);
  assert.deepEqual(said.opengraph['article:published_time'], ['2026-09-28T07:30:00+02:00']);
  assert.deepEqual(said.twitter['twitter:card'], ['summary']);
  assert.equal(said.jsonld.length, 1);
  assert.equal(said.jsonld[0]?.['@type'], 'Article');
  assert.deepEqual(said.headings, [
    { level: 1, text: ESSAY, id: null },
    { level: 2, text: 'The rounds', id: 'rounds' },
    { level: 2, text: 'The ledger', id: 'ledger' },
  ]);
  // The essay, and none of what stands around it.
  assert.ok(said.text.includes('Forty-two lamps stood along the quay'), said.text.slice(0, 300));
  assert.ok(said.text.includes('The hill, last of all.'));
  for (const furniture of ['Subscribe to the letter', 'Its pages are generated', 'Articles']) {
    assert.ok(!said.text.includes(furniture), `${engine}: the text has "${furniture}"`);
  }
  const markdown = said.markdown ?? '';
  assert.ok(markdown.startsWith(`# ${ESSAY}`), markdown.slice(0, 200));
  assert.ok(markdown.includes('## The rounds'));
  assert.ok(markdown.includes('```toml\n[lamps]\nquay = 12\nmarket = 9\n```'), markdown);
  assert.ok(markdown.includes('| Street | Lamps |'), markdown);
  assert.ok(markdown.includes('1. The quay, before the boats come in.'), markdown);
  assert.ok(markdown.includes(`[the first article](${site}/site/a)`), markdown);
  const links = said.links ?? [];
  const own = links.find((link) => link.url === `${site}/site/a`);
  assert.equal(own?.same_site, true);
  assert.equal(own?.text, 'the first article');
  const other = links.find((link) => link.url === 'http://elsewhere.kage-fixture.example/lamps');
  assert.equal(other?.same_site, false);
  assert.deepEqual(other?.rel, ['nofollow']);
  assert.equal(said.thin_content, false);
  assert.ok(said.word_count > 150, `${engine}: ${said.word_count} words`);
  assert.equal(said.word_count, said.text.split(/\s+/).filter(Boolean).length);
  // Only the extraction and the headers were asked for.
  assert.equal(first.dom_html ?? null, null);

  // The same request again is no visit: the stored result of the first
  // one, under an id of its own, and it says since when.
  const hitFrom = Date.now();
  const second = await finished(client, asking);
  const hitUntil = Date.now();
  assert.equal(second.cached, true, `${engine}: the second request was a visit`);
  assert.equal(cachedAt(second)?.getTime(), new Date(first.created_at).getTime());
  assert.notEqual(second.id, first.id);
  assert.deepEqual(second.attempts, []);
  assert.equal(second.outcome, 'ok');
  assert.equal(second.status, 200);
  assert.deepEqual(extractOf(second), said);
  assert.deepEqual(second.persona, first.persona);
  assert.deepEqual(second.robots, first.robots);
  const stored = await readResult(client, second.id);
  assert.ok(isResult(stored));
  assert.equal(stored.cached, true);
  assert.deepEqual(extractOf(stored), said);

  return {
    engine,
    path: `/read/essay?reader=${engine}`,
    reader: first.persona.id,
    words: said.word_count,
    links: links.length,
    first: first.id,
    hit: second.id,
    cached_at: second.cached_at,
    // While the cache answered, by this client's clock.
    hit_from: hitFrom,
    hit_until: hitUntil,
    hit_ms: hitUntil - hitFrom,
    visit_ms: first.timing.total_ms,
  };
}

/**
 * A page whose script nests its text ten thousand elements deep (F97), read
 * at level 2 as the finding read it: Firefox hands the document back as
 * deep as the script built it, and the server reads it whole on a worker's
 * stack. The page writes the depth it measured into its title. Only
 * Firefox is asked: Chromium's sidecar does not answer the navigation of a
 * page its script nests even two thousand elements deep within the
 * server's bound, and the fetch ends `sidecar_error` (a finding of its
 * own, outside F97).
 */
async function deep(client: KageClient, site: string) {
  const engine: EngineName = 'firefox';
  const fetched = await finished(client, {
    url: `${site}/deep?reader=${engine}`,
    level: 2,
    timeout_ms: 60_000,
    include: ['extract'],
    persona: { engines: [engine] },
  });
  assert.equal(fetched.outcome, 'ok', `${engine}: ${fetched.reason ?? fetched.outcome}`);
  assert.equal(fetched.level, 2);
  assert.equal(fetched.persona?.engine, engine);
  const said = extractOf(fetched);
  const depth = Number(/^Deep (\d+)$/.exec(said.title ?? '')?.[1] ?? Number.NaN);
  assert.ok(depth >= 10_000, `${engine}: the page says "${said.title ?? ''}"`);
  assert.equal(said.word_count, 100, `${engine}: ${said.text.slice(0, 200)}`);
  assert.ok(said.text.startsWith('word0 word1 '), said.text.slice(0, 200));
  assert.ok(said.text.endsWith(' word99'), said.text.slice(-200));
  assert.equal(said.thin_content, false);
  return { engine, reader: fetched.persona.id, depth, words: said.word_count, id: fetched.id };
}

/** Another include set is another request: a visit, though the cache holds the page. */
async function otherInclude(client: KageClient, site: string, engine: EngineName) {
  const third = await finished(client, { ...essayRequest(site, engine), include: ['extract'] });
  assert.equal(third.cached, false, `${engine}: another include set was served from the cache`);
  assert.equal(third.attempts.length, 1);
  assert.equal(extractOf(third).title, `${ESSAY} | The reading room`);
  return third.id;
}

/** A page the enforced file allows, and one it disallows. */
async function enforced(client: KageClient, rules: string, engine: EngineName) {
  const reading = { persona: { engines: [engine] }, ...QUICK } as const;
  const started = Date.now();
  const allowed = await finished(client, {
    ...reading,
    persona: { engines: [engine] },
    url: `${rules}/site/a?reader=${engine}`,
  });
  assert.equal(allowed.outcome, 'ok', `${engine}: ${allowed.reason ?? allowed.outcome}`);
  assert.equal(allowed.persona?.engine, engine);
  assert.deepEqual(allowed.robots, {
    fetched: true,
    allowed: true,
    matched_rule: null,
    crawl_delay: null,
  });
  assert.equal(robotsDenial(allowed), undefined);

  // What the file disallows is a result, not an error, and no visit.
  const denied = await finished(client, {
    ...reading,
    persona: { engines: [engine] },
    url: `${rules}/site/private?reader=${engine}`,
  });
  assert.equal(denied.outcome, 'robots_denied');
  assert.deepEqual(robotsDenial(denied), { rule: 'Disallow: /site/private' });
  assert.deepEqual(denied.attempts, []);
  assert.equal(denied.persona ?? null, null);
  assert.equal(denied.status ?? null, null);
  assert.equal(denied.robots?.fetched, true);
  assert.equal(denied.robots?.allowed, false);
  const stored = await readResult(client, denied.id);
  assert.ok(isResult(stored));
  assert.equal(stored.outcome, 'robots_denied');

  return {
    engine,
    reader: allowed.persona.id,
    // How long the first fetch of the origin took, the scout's visit
    // included, and the ones after it.
    allowed_ms: Date.parse(allowed.created_at) - started,
    waited_ms: allowed.pacing?.waited_ms ?? null,
    denied: denied.id,
  };
}

/** A crawl of the site under the name whose robots.txt is enforced. */
async function crawl(client: KageClient, rules: string, engine: EngineName) {
  const origin = new URL(rules).origin;
  const { job } = await createCrawl(client, {
    seeds: [`${rules}/site/about`],
    use_sitemaps: true,
    max_depth: 1,
    // The seed, what it links to, and what the sitemaps name.
    allow: ['/site/(about|annex|private|a|b|c|only-1|only-2)$'],
    defaults: { ...QUICK, include: ['extract'], persona: { engines: [engine] } },
  });
  const done = await waitForJob(client, job.id, { timeoutMs: 900_000, everyMs: 2000 });
  assert.equal(done.state, 'succeeded', JSON.stringify(done));
  const progress = crawlProgress(done);
  assert.equal(progress.ended, 'completed');
  const pages = await readCrawlPages(client, job.id, { results: true, pageSize: 200 });
  const read = pages.map((page) => new URL(page.url).pathname).sort();

  // The file was read first and is listed with the sitemaps; the sitemap
  // it names is the one that was asked for, not the guess.
  const state = (path: string) => progress.sitemaps.find((one) => one.url === `${origin}${path}`);
  assert.deepEqual(progress.sitemaps[0], {
    site: 'kage.test',
    url: `${origin}/robots.txt`,
    state: 'read',
    urls: 1,
  });
  assert.equal(state('/sitemaps/index.xml')?.state, 'index', JSON.stringify(progress.sitemaps));
  assert.equal(state('/sitemap.xml'), undefined, JSON.stringify(progress.sitemaps));
  assert.equal(state('/sitemaps/pages.xml')?.state, 'read');
  const packed = state('/sitemaps/more.xml.gz');
  const expected = ['/site/a', '/site/about', '/site/annex', '/site/b'];
  if (packed?.state === 'read') {
    // The site's scout is a Firefox: it reads the gzipped sitemap too.
    expected.push('/site/c', '/site/only-1', '/site/only-2');
  } else {
    assert.equal(packed?.state, 'sitemap_unreadable', JSON.stringify(packed));
  }
  assert.deepEqual(read, expected.sort());

  // What the file disallows was never made a page: it is counted.
  assert.equal(progress.skipped['robots_denied'], 1, JSON.stringify(progress.skipped));
  assert.ok(!read.includes('/site/private'));
  assert.equal(done.items.total, pages.length);
  assert.equal(done.items.succeeded, pages.length);

  // One persona of the engine read it; every page's result has the
  // verdict and what the page says.
  const readers = new Set<string>();
  for (const { item, url } of pages) {
    const result = item.result;
    assert.ok(result !== undefined && result !== null, `${url} has no result`);
    assert.equal(result.outcome, 'ok', url);
    assert.equal(result.cached, false);
    assert.deepEqual(
      result.robots,
      { fetched: true, allowed: true, matched_rule: null, crawl_delay: null },
      url,
    );
    assert.equal(extractOf(result).thin_content, false, url);
    assert.ok((extractOf(result).title ?? '').length > 0, url);
    assert.equal(result.persona?.engine, engine, url);
    readers.add(result.persona.id);
  }
  assert.equal(readers.size, 1, `one visitor reads a site: ${JSON.stringify([...readers])}`);
  const about = pages.find((page) => page.url.endsWith('/site/about'));
  assert.equal(about?.page.found_by, 'seed');
  assert.equal(pages.find((page) => page.url.endsWith('/site/a'))?.page.found_by, 'sitemap');

  return {
    engine,
    id: job.id,
    reader: [...readers][0],
    pages: read,
    sitemaps: progress.sitemaps,
    skipped: progress.skipped,
    created_at: done.created_at,
    finished_at: done.finished_at,
  };
}

async function main(): Promise<unknown> {
  const client = createKageClient({ baseUrl: env('KAGE_API_URL'), token: env('KAGE_API_KEY') });
  const site = env('KAGE_E2E_SITE');
  const rules = env('KAGE_E2E_RULES');

  // One persona of each engine reads the site before anybody is asked to
  // scout it, so that the scout is the persona that is left.
  const essays = [];
  for (const engine of ENGINES) {
    essays.push(await essay(client, site, engine));
  }
  // The page nested ten thousand elements deep, by the Firefox reader.
  const deeps = [await deep(client, site)];

  // What a request may not ask of a site: a robots.txt enforced that the
  // policy never asks for, less than the policy enforces, and a longer
  // life in the cache than results have.
  const refusals = [
    await refusal(client, { url: `${site}/read/essay`, level: 3, robots: 'enforce' }),
    await refusal(client, { url: `${rules}/site/private`, level: 3, robots: 'ignore' }),
    await refusal(client, { url: `${rules}/site/private`, level: 3, robots: 'report' }),
    await refusal(client, { url: `${site}/read/essay`, level: 3, cache_ttl_s: 86_401 }),
    await refusal(client, { url: `${site}/read/essay`, level: 3, extract: { links: false } }),
  ];

  const pages = [];
  for (const engine of ENGINES) {
    pages.push(await enforced(client, rules, engine));
  }
  const crawls = [];
  for (const engine of ENGINES) {
    crawls.push(await crawl(client, rules, engine));
  }
  // Asked last, when the site has its scout: until then exactly one
  // persona of each engine has read the site, and the third is left to
  // ask for what no reader asks for.
  const visits = [];
  for (const engine of ENGINES) {
    visits.push(await otherInclude(client, site, engine));
  }
  // The reader of an engine is one persona throughout: the scout took
  // none of them.
  assert.equal(deeps[0]?.reader, essays[1]?.reader, 'firefox: another reader of the deep page');
  for (const [index, engine] of ENGINES.entries()) {
    assert.equal(pages[index]?.reader, essays[index]?.reader, `${engine}: another reader`);
    assert.equal(crawls[index]?.reader, essays[index]?.reader, `${engine}: another reader`);
  }

  return { essays, deep: deeps, refusals, enforced: pages, crawls, visits };
}

try {
  process.stdout.write(`${JSON.stringify(await main())}\n`);
} catch (error) {
  process.stderr.write(
    `${error instanceof Error ? (error.stack ?? error.message) : String(error)}\n`,
  );
  process.exitCode = 1;
}
