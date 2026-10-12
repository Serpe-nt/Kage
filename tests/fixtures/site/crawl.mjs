// The part of the fixture site a crawl reads (spec §13): a small site of
// linked pages under /site/, with what a crawler has to get right. Nothing
// here is a real site and every word of its text is generated.
//
//   /site/                 the home page: links to the sections, a link with
//                          campaign tags, one with a fragment, one to another
//                          site, one no reader can see, a mail address, a
//                          script link and a file
//   /site/articles         the listing: every article, the two copies, the
//                          two near-copies, the first page of the deep chain,
//                          an address that moved and one that is gone
//   /site/a, /b, /c        articles; /site/a links one page deeper
//   /site/dup-1, /dup-2    one text under two addresses
//   /site/near-1, /near-2  one text with another date line and one sentence
//                          changed
//   /site/deep/1 .. /6     a chain: each page links only to the next
//   /site/tall             a page many screens long
//   /site/about            links to a page on another host of the same site
//   /site/only-1, /only-2  pages nothing links to: only the sitemap names them
//   /site/slow/<n>?ms=     a chain of pages that take their time
//   /sitemap.xml           a sitemap index: one plain sitemap, one gzipped
//   /robots.txt            names the sitemap and disallows /site/private
import { gzipSync } from 'node:zlib';

const WORDS = (
  'harbour lantern meadow copper window orchard river quiet letter garden morning ' +
  'bridge winter candle market shadow feather island silver thunder pocket village ' +
  'paper mountain kitchen violin summer stone ribbon forest signal weather button ' +
  'journey mirror velvet engine blanket compass autumn ladder marble whisper anchor ' +
  'cellar pepper harvest saddle timber wagon pillow lantern cobble thimble barley ' +
  'chimney willow gravel sparrow kettle parlour bramble orchard saffron cinder'
).split(' ');

/** A small seeded generator (mulberry32): the same page says the same thing every time. */
function random(seed) {
  let state = 0;
  for (const char of seed) state = (Math.imul(state, 31) + char.charCodeAt(0)) >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function sentence(next) {
  const length = 8 + Math.floor(next() * 10);
  const words = Array.from({ length }, () => WORDS[Math.floor(next() * WORDS.length)]);
  const text = words.join(' ');
  return `${text[0].toUpperCase()}${text.slice(1)}.`;
}

/** Paragraphs of generated text, the same for the same seed. */
function paragraphs(seed, count) {
  const next = random(seed);
  return Array.from({ length: count }, () => {
    const sentences = 3 + Math.floor(next() * 4);
    return Array.from({ length: sentences }, () => sentence(next)).join(' ');
  });
}

const NAV =
  '<nav id="nav"><a href="/site/">Home</a> <a href="/site/articles">Articles</a> <a href="/site/about">About</a></nav>';
const FOOTER =
  '<footer id="footer"><p>The reading room is a fixture. Its pages are generated and say nothing.</p></footer>';

function layout(title, body) {
  return `<!doctype html><html lang="en"><head><meta charset="utf-8"><title>${title}</title><style>main{max-width:40em;margin:auto;line-height:1.5}</style></head><body>${NAV}<main><h1>${title}</h1>${body}</main>${FOOTER}</body></html>`;
}

const text = (seed, count) =>
  paragraphs(seed, count)
    .map((paragraph) => `<p>${paragraph}</p>`)
    .join('\n');

const links = (...items) =>
  `<ul class="links">${items.map(([href, label]) => `<li><a href="${href}">${label}</a></li>`).join('')}</ul>`;

const article = (name, title, extra = '') => layout(title, `${text(`article-${name}`, 5)}${extra}`);

/** The near-copies: one text, another date line, one sentence of another kind. */
function near(which) {
  const body = paragraphs('near', 6);
  const changed = which === 1 ? body : body.map((p, i) => (i === 3 ? `${sentence(random('near-other'))} ${p}` : p));
  const date = which === 1 ? 'Published on 3 March.' : 'Updated on 9 October.';
  return layout(
    'A walk along the harbour',
    `<p class="date">${date}</p>${changed.map((p) => `<p>${p}</p>`).join('\n')}`,
  );
}

const DEEPEST = 6;
const SLOWEST = 40;

/** The other host of the same site: `lenient.` in place of `fixture.`, the port kept. */
const sibling = (host) => (host.startsWith('fixture.') ? `lenient.${host.slice('fixture.'.length)}` : host);

const pages = {
  '/site/': () =>
    layout(
      'The reading room',
      `${text('home', 3)}
${links(
  ['/site/articles', 'All articles'],
  ['/site/a?utm_source=home&utm_medium=fixture', 'This week: the first article'],
  ['/site/about#team', 'Who writes here'],
  ['/site/tall', 'The long read'],
  ['http://offsite.example.test/hello', 'A friend of the house'],
  ['mailto:nobody@fixture.invalid', 'Write to us'],
  ['javascript:void(0)', 'Open the menu'],
  ['/site/files/report.pdf', 'The yearly report'],
)}
<a id="trap" href="/site/trap" style="display:none">Do not follow</a>
<p hidden><a id="trap-hidden" href="/site/trap?from=hidden">Nor this</a></p>`,
    ),
  '/site/articles': () =>
    layout(
      'All articles',
      `${text('articles', 2)}
${links(
  ['/site/a', 'The first article'],
  ['/site/b', 'The second article'],
  ['/site/c', 'The third article'],
  ['/site/dup-1', 'A note, printed twice'],
  ['/site/dup-2', 'A note, printed twice (again)'],
  ['/site/near-1', 'A walk along the harbour'],
  ['/site/near-2', 'A walk along the harbour, revised'],
  ['/site/deep/1', 'A story in six parts'],
  ['/site/moved', 'The first article at its old address'],
  ['/site/missing', 'An article that was taken down'],
)}`,
    ),
  '/site/a': () => article('a', 'The first article', links(['/site/a/notes', 'Notes on the first article'])),
  '/site/a/notes': () => article('a-notes', 'Notes on the first article'),
  '/site/b': () => article('b', 'The second article'),
  '/site/c': () => article('c', 'The third article'),
  '/site/dup-1': () => layout('A note, printed twice', text('dup', 5)),
  '/site/dup-2': () => layout('A note, printed twice', text('dup', 5)),
  '/site/near-1': () => near(1),
  '/site/near-2': () => near(2),
  // Says how far it was read when it is left, for whoever measures a visit.
  '/site/tall': () =>
    layout(
      'The long read',
      `${text('tall', 90)}<script>addEventListener('pagehide', () => navigator.sendBeacon('/__kage/report', JSON.stringify({ page: 'tall', scrollY: Math.round(scrollY), scrollHeight: document.documentElement.scrollHeight, innerHeight })));</script>`,
    ),
  '/site/about': (host) =>
    layout(
      'About the reading room',
      `${text('about', 3)}
${links([`http://${sibling(host)}/site/annex`, 'The annex, on its own host'], ['/site/private', 'The back office'])}
<h2 id="team">The team</h2>${text('team', 2)}`,
    ),
  '/site/annex': () => article('annex', 'The annex'),
  '/site/private': () => article('private', 'The back office'),
  '/site/only-1': () => article('only-1', 'A page only the sitemap knows'),
  '/site/only-2': () => article('only-2', 'Another page only the sitemap knows'),
  '/site/trap': () => article('trap', 'A page no reader can reach'),
};

function sitemapIndex(host) {
  return `<?xml version="1.0" encoding="UTF-8"?>
<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <sitemap><loc>http://${host}/sitemaps/pages.xml</loc><lastmod>2026-10-01</lastmod></sitemap>
  <sitemap><loc>http://${host}/sitemaps/more.xml.gz</loc><lastmod>2026-10-05T08:30:00+00:00</lastmod></sitemap>
</sitemapindex>
`;
}

function urlset(host, entries) {
  const urls = entries
    .map(
      ([path, lastmod]) =>
        `  <url><loc>http://${host}${path}</loc>${lastmod ? `<lastmod>${lastmod}</lastmod>` : ''}<changefreq>weekly</changefreq></url>`,
    )
    .join('\n');
  return `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
${urls}
</urlset>
`;
}

/**
 * Answers a request for the crawl part of the site.
 *
 * @returns whether the path was one of its own.
 */
export function crawlRoute(req, res, url) {
  const host = req.headers.host ?? 'fixture.invalid';
  const send = (status, type, body, headers = {}) => {
    res.writeHead(status, { 'content-type': type, ...headers });
    res.end(body);
  };
  const html = (body, status = 200, headers = {}) => send(status, 'text/html; charset=utf-8', body, headers);
  const path = url.pathname;

  if (path === '/robots.txt') {
    send(200, 'text/plain; charset=utf-8', `User-agent: *\nDisallow: /site/private\n\nSitemap: http://${host}/sitemap.xml\n`);
    return true;
  }
  if (path === '/sitemap.xml') {
    send(200, 'application/xml; charset=utf-8', sitemapIndex(host));
    return true;
  }
  if (path === '/sitemaps/pages.xml') {
    send(
      200,
      'application/xml; charset=utf-8',
      urlset(host, [
        ['/site/', '2026-10-01'],
        ['/site/a', '2026-09-28'],
        ['/site/b', null],
      ]),
    );
    return true;
  }
  if (path === '/sitemaps/more.xml.gz') {
    // A gzipped file, not a gzipped transfer: the body is the archive.
    send(
      200,
      'application/gzip',
      gzipSync(
        urlset(host, [
          ['/site/only-1', '2026-10-05T08:30:00+00:00'],
          ['/site/only-2', '2026-10-04'],
          ['/site/c', '2026-09-30'],
        ]),
      ),
    );
    return true;
  }
  if (path === '/site') {
    send(301, 'text/plain', '', { location: '/site/' });
    return true;
  }
  if (path === '/site/moved') {
    send(301, 'text/plain', '', { location: '/site/a' });
    return true;
  }
  if (path === '/site/files/report.pdf') {
    send(200, 'application/pdf', '%PDF-1.4\n% a fixture, not a document\n');
    return true;
  }
  const deep = /^\/site\/deep\/(\d+)$/.exec(path);
  if (deep) {
    const part = Number(deep[1]);
    if (part < 1 || part > DEEPEST) return false;
    const next = part < DEEPEST ? links([`/site/deep/${part + 1}`, `Part ${part + 1}`]) : '<p>The end.</p>';
    html(layout(`A story in six parts: part ${part}`, `${text(`deep-${part}`, 4)}${next}`));
    return true;
  }
  const slow = /^\/site\/slow\/(\d+)$/.exec(path);
  if (slow) {
    const part = Number(slow[1]);
    if (part < 1 || part > SLOWEST) return false;
    const ms = Math.min(20_000, Number(url.searchParams.get('ms') ?? '500'));
    const query = url.search;
    const next = part < SLOWEST ? links([`/site/slow/${part + 1}${query}`, `Slow page ${part + 1}`]) : '';
    setTimeout(() => html(layout(`Slow page ${part}`, `${text(`slow-${part}`, 3)}${next}`)), ms);
    return true;
  }
  const page = pages[path];
  if (page === undefined) return false;
  html(page(host));
  return true;
}
