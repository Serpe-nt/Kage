#!/usr/bin/env node
// The fixture site (spec §21): a small web site that tests navigate instead
// of a real one. It has no dependencies, so the sidecar tests, the compose
// end-to-end suite and a person with `node` can all start it:
//
//   node tests/fixtures/site/server.mjs [--port 0] [--host 127.0.0.1]
//
// It prints one line of JSON with the port it listens on. Besides its pages
// it keeps a log of every request it received, with the header lines in
// wire order, and of everything pages reported back; tests read both under
// /__kage/. Nothing here is a real site and nothing here contains personal
// data.
import { readFileSync } from 'node:fs';
import http from 'node:http';
import { fileURLToPath } from 'node:url';

import { crawlRoute } from './crawl.mjs';

const args = process.argv.slice(2);
const option = (name, fallback) => {
  const index = args.indexOf(`--${name}`);
  return index >= 0 && args[index + 1] !== undefined ? args[index + 1] : fallback;
};
const host = option('host', '127.0.0.1');
const port = Number(option('port', '0'));

const file = (name) => readFileSync(fileURLToPath(new URL(name, import.meta.url)), 'utf8');
const PROBE_PAGE = file('./probe/index.html');
const PROBE_FRAME = file('./probe/frame.html');
const PROBE_WORKER = file('./probe/worker.js');
const PROBE_SCOPE = file('./probe/scope.js');
const INPUT_PAGE = file('./input/index.html');
const ACTIVATION_PAGE = file('./activation/index.html');

/** Every request, oldest first. */
const requests = [];
/** Everything pages posted to /__kage/report, oldest first. */
const reports = [];
/** How often /twice was served since the last reset. */
let twice = 0;

const page = (title, body, head = '') =>
  `<!doctype html><html lang="en"><head><meta charset="utf-8"><title>${title}</title>${head}</head><body>${body}</body></html>`;

const HOME = page(
  'Fixture home',
  `<h1 id="title">Fixture home</h1>
<p id="visits"></p>
<ul id="items"></ul>
<p><a id="next" href="/listing?page=2">next</a></p>
<img alt="" src="/pixel.png" width="1" height="1">
<script src="/app.js"></script>`,
);

const APP_JS = `(() => {
  const visits = (document.cookie.match(/(?:^|; )visits=(\\d+)/) || [])[1] || '0';
  document.getElementById('visits').textContent = 'visits: ' + visits;
  fetch('/api/items?source=fetch').then((r) => r.json()).then((data) => {
    for (const item of data.items) {
      const li = document.createElement('li');
      li.textContent = item.name;
      document.getElementById('items').appendChild(li);
    }
    document.body.setAttribute('data-items', String(data.items.length));
  });
  const xhr = new XMLHttpRequest();
  xhr.open('GET', '/api/time');
  xhr.setRequestHeader('X-Requested-With', 'XMLHttpRequest');
  xhr.send();
  fetch('/api/text');
  fetch('/api/binary');
})();`;

// A page that tells the server everything a script can notice about what is
// done to it from outside: events and DOM changes.
const EVENTS_PAGE = page(
  'Events',
  `<h1>Events</h1><p style="height:2600px">tall</p>
<script>
(() => {
  // The query string tells one load of this page from the next.
  const run = location.search.slice(1);
  const send = (what) => navigator.sendBeacon('/__kage/report', JSON.stringify({ page: 'events', run, what }));
  for (const type of ['resize', 'scroll', 'beforeprint', 'afterprint', 'visibilitychange', 'blur', 'focus', 'pagehide'])
    addEventListener(type, () => send(type));
  document.body.setAttribute('data-ready', '1');
  new MutationObserver((records) => {
    for (const record of records) send('mutation:' + record.target.nodeName);
  }).observe(document.documentElement, { childList: true, subtree: true, attributes: true });
  send('ready');
})();
</script>`,
);

const CHALLENGE_PAGE = page(
  'Just a moment...',
  `<h1>Checking your browser before accessing the fixture</h1>
<script>setTimeout(() => { document.cookie = 'fixture_clearance=1; path=/'; location.reload(); }, 1200);</script>`,
);

const STUCK_CHALLENGE_PAGE = page(
  'Just a moment...',
  '<h1>Checking your browser before accessing the fixture</h1><p>This one never resolves.</p>',
);

const STORAGE_PAGE = page(
  'Storage',
  `<h1>Storage</h1><script>
localStorage.setItem('seen', '1'); localStorage.setItem('theme', 'dark');
sessionStorage.setItem('step', '2');
document.body.setAttribute('data-ready', '1');
</script>`,
);

const CONSOLE_PAGE = page(
  'Console',
  `<h1>Console</h1><img alt="" src="/missing-image.png">
<script>
// What a detector does to find a debugger that serializes console arguments.
window.__stackReads = 0;
const probe = new Error('probe');
Object.defineProperty(probe, 'stack', { get() { window.__stackReads += 1; return ''; } });
console.debug(probe); console.error('an error text', probe);
setTimeout(() => navigator.sendBeacon('/__kage/report', JSON.stringify({ page: 'console', stackReads: window.__stackReads })), 300);
</script>`,
);

// Opens a WebRTC connection against the STUN server named in the query
// and reports which candidates were gathered. A test listens on that
// address to see whether anything left around the proxy.
const WEBRTC_PAGE = page(
  'WebRTC',
  `<h1>WebRTC</h1><script>
(async () => {
  const stun = new URLSearchParams(location.search).get('stun');
  const kinds = [];
  try {
    const peer = new RTCPeerConnection({ iceServers: [{ urls: 'stun:' + stun }] });
    peer.onicecandidate = (event) => { if (event.candidate) kinds.push(event.candidate.type); };
    peer.createDataChannel('probe');
    await peer.setLocalDescription(await peer.createOffer());
    await new Promise((resolve) => setTimeout(resolve, 2500));
    peer.close();
  } catch (error) {
    kinds.push('error:' + error);
  }
  navigator.sendBeacon('/__kage/report', JSON.stringify({ page: 'webrtc', candidates: kinds }));
  document.body.setAttribute('data-done', '1');
})();
</script>`,
);

// A page that changes its address without loading another document, as a
// single-page application does: one document under two addresses, and the
// browser's way back leads from one to the other without a request.
const SPA_PAGE = page(
  'Single page',
  `<h1 id="view">first view</h1>
<p><button id="push" type="button">second view</button></p>
<script>
(() => {
  const show = () => {
    document.getElementById('view').textContent = location.pathname === '/spa' ? 'first view' : 'second view';
  };
  document.getElementById('push').addEventListener('click', () => {
    history.pushState({ view: 2 }, '', '/spa/second?view=2');
    show();
  });
  addEventListener('popstate', show);
})();
</script>`,
);

// A page that tells the server how it was left (spec §13, follow-up F28):
// what its visitor did on it, and what the page saw before it went away.
// A link clicked by a hand shows the pointer arriving, a button going down
// and up and a trusted click; an address typed into the browser shows none
// of that. The run in the query tells one load from the next.
const LEAVE_PAGE = page(
  'Leave',
  `<h1 id="title">A page to leave</h1>
<p>${'A line of text to rest the eyes on. '.repeat(12)}</p>
<p><a id="next" href="/landing?from=leave">The next page</a></p>
<p style="height:1400px">below</p>
<p><a id="below" href="/landing?from=below">A link further down</a></p>
<script>
(() => {
  const run = new URLSearchParams(location.search).get('run') || '';
  const events = [];
  let moves = 0, firstMove = null, lastMove = null, wheels = 0;
  const note = (type, more) => events.push({ type, t: Math.round(performance.now()), ...more });
  addEventListener('mousemove', (event) => {
    moves += 1;
    const t = Math.round(performance.now());
    firstMove ??= t;
    lastMove = t;
    if (!event.isTrusted) note('untrusted-mousemove');
  }, true);
  addEventListener('wheel', () => { wheels += 1; }, { capture: true, passive: true });
  for (const type of ['mousedown', 'mouseup', 'click', 'keydown', 'keyup'])
    addEventListener(type, (event) => note(type, { trusted: event.isTrusted, target: event.target && event.target.id || null, key: event.key || null }), true);
  document.documentElement.addEventListener('mouseleave', () => note('mouseleave'));
  for (const type of ['blur', 'focus', 'beforeunload'])
    addEventListener(type, () => note(type));
  document.addEventListener('visibilitychange', () => note('visibilitychange', { state: document.visibilityState }));
  addEventListener('pagehide', (event) => {
    note('pagehide', { persisted: event.persisted });
    navigator.sendBeacon('/__kage/report', JSON.stringify({
      page: 'leave', run, events, moves, firstMove, lastMove, wheels,
      scrollY: Math.round(scrollY), activation: navigator.userActivation ? navigator.userActivation.hasBeenActive : null,
    }));
  });
  document.body.setAttribute('data-ready', '1');
})();
</script>`,
);

// The 1x1 transparent PNG.
const PIXEL = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=',
  'base64',
);

const headerPairs = (req) => {
  const pairs = [];
  for (let index = 0; index < req.rawHeaders.length; index += 2) {
    pairs.push([req.rawHeaders[index], req.rawHeaders[index + 1]]);
  }
  return pairs;
};

const cookieOf = (req, name) =>
  (req.headers.cookie ?? '')
    .split(/;\s*/)
    .map((part) => part.split('='))
    .find(([key]) => key === name)?.[1] ?? null;

const send = (res, status, type, body, headers = {}) => {
  res.writeHead(status, { 'content-type': type, ...headers });
  res.end(body);
};
const html = (res, body, headers = {}, status = 200) =>
  send(res, status, 'text/html; charset=utf-8', body, headers);
const json = (res, value, headers = {}) =>
  send(res, 200, 'application/json', JSON.stringify(value), headers);

/** Puts the headers of the request that fetched a document into the document. */
const withHeaders = (template, req) =>
  template.replace('/*KAGE_REQUEST_HEADERS*/null', JSON.stringify(headerPairs(req)));

const routes = {
  '/': (req, res) => {
    const visits = Number(cookieOf(req, 'visits') ?? '0') + 1;
    html(res, HOME, { 'set-cookie': `visits=${visits}; Path=/; Max-Age=86400; SameSite=Lax` });
  },
  '/app.js': (_req, res) => send(res, 200, 'text/javascript', APP_JS, { 'cache-control': 'max-age=600' }),
  '/pixel.png': (_req, res) => send(res, 200, 'image/png', PIXEL, { 'cache-control': 'max-age=600' }),
  '/api/items': (_req, res) => json(res, { items: [{ name: 'alpha' }, { name: 'beta' }, { name: 'gamma' }] }),
  '/api/time': (_req, res) => json(res, { now: '2026-10-09T12:00:00Z' }, { 'content-type': 'application/json; charset=utf-8' }),
  '/api/text': (_req, res) => send(res, 200, 'text/plain', 'plain text, not wanted by a JSON filter'),
  '/api/binary': (_req, res) => send(res, 200, 'application/octet-stream', Buffer.from([0x00, 0xff, 0xfe, 0x80])),
  '/api/large': (_req, res) => json(res, { blob: 'x'.repeat(50_000) }),
  '/listing': (req, res, url) =>
    html(res, page(`Listing, page ${url.searchParams.get('page') ?? '1'}`, `<h1>Listing</h1><p id="referer">${req.headers.referer ?? 'none'}</p>`)),
  '/redirect/a': (_req, res) => send(res, 302, 'text/plain', '', { location: '/redirect/b' }),
  '/redirect/b': (_req, res) => send(res, 301, 'text/plain', '', { location: '/landing?from=redirect' }),
  '/redirect/loopback': (_req, res) => send(res, 302, 'text/plain', '', { location: 'http://127.0.0.1:9/never' }),
  '/landing': (_req, res) => html(res, page('Landing', '<h1 id="landing">Landed</h1>'), { 'x-fixture': 'landing' }),
  '/slow': (_req, res, url) => {
    setTimeout(() => html(res, page('Slow', '<h1>Slow</h1>')), Number(url.searchParams.get('ms') ?? '1000'));
  },
  '/late': (_req, res) =>
    html(res, page('Late', '<h1>Late</h1><script>setTimeout(() => { const p = document.createElement("p"); p.id = "late"; p.textContent = "here"; document.body.appendChild(p); }, 700);</script>')),
  '/events.html': (_req, res) => html(res, EVENTS_PAGE),
  '/challenge': (req, res) =>
    cookieOf(req, 'fixture_clearance') === '1'
      ? html(res, page('Behind the challenge', '<h1 id="content">Content behind the challenge</h1>'))
      : html(res, CHALLENGE_PAGE, {}, 403),
  '/challenge/stuck': (_req, res) => html(res, STUCK_CHALLENGE_PAGE, {}, 403),
  '/spa': (_req, res) => html(res, SPA_PAGE, { 'x-fixture': 'spa' }),
  // One address, two documents: the page links to itself with a campaign
  // tag, and the document that arrives takes the tag out of its address,
  // as many sites' scripts do. The history of the tab then holds two
  // entries with the same address, and the way back to the first document,
  // which the browser kept, changes neither the address nor anything on
  // the wire. Each answer says which one it is.
  '/twice': (_req, res) => {
    twice += 1;
    html(
      res,
      page(
        'Twice',
        `<h1 id="view">served ${twice}</h1><p><a id="tagged" href="/twice?utm_source=fixture">again</a></p>
<script>if (location.search) history.replaceState(null, '', location.pathname);</script>`,
      ),
      { 'x-fixture': `twice-${twice}` },
    );
  },
  '/storage.html': (_req, res) => html(res, STORAGE_PAGE),
  '/console.html': (_req, res) => html(res, CONSOLE_PAGE),
  '/webrtc.html': (_req, res) => html(res, WEBRTC_PAGE),
  '/cached.html': (_req, res) => html(res, page('Cached', '<h1>Cached for ten minutes</h1>'), { 'cache-control': 'max-age=600' }),
  '/big.html': (_req, res) => html(res, page('Big', `<p>${'ä'.repeat(40_000)}</p>`)),
  '/set-cookies': (_req, res) =>
    html(res, page('Cookies', '<h1>Cookies set</h1>'), {
      'set-cookie': [
        'session_only=s1; Path=/',
        'persistent=p1; Path=/; Max-Age=3600; HttpOnly; SameSite=Strict',
        'scoped=c1; Path=/account; Max-Age=3600',
      ],
    }),
  '/cookies': (req, res) => json(res, { cookie: req.headers.cookie ?? '' }),
  '/download': (_req, res) =>
    send(res, 200, 'application/octet-stream', 'file body', { 'content-disposition': 'attachment; filename="file.bin"' }),
  '/status/404': (_req, res) => html(res, page('Not found', '<h1>404</h1>'), {}, 404),
  '/headers': (req, res) => json(res, { headers: headerPairs(req) }),

  // The coherence probe (spec §6.2).
  '/probe/': (req, res) =>
    html(res, withHeaders(PROBE_PAGE, req), {
      'accept-ch': 'Sec-CH-UA-Platform-Version, Sec-CH-UA-Arch, Sec-CH-UA-Bitness, Sec-CH-UA-Full-Version-List',
    }),
  '/probe/frame.html': (req, res) => html(res, withHeaders(PROBE_FRAME, req)),
  '/probe/worker.js': (req, res) => send(res, 200, 'text/javascript', withHeaders(PROBE_WORKER, req)),
  '/probe/scope.js': (_req, res) => send(res, 200, 'text/javascript', PROBE_SCOPE),
  '/probe/headers': (req, res) => json(res, { headers: headerPairs(req) }),

  // A results page as a search engine shows one (spec §10.4, the search_engine
  // entry): every `r` of the query is a result, in order, with a line of text.
  '/search': (_req, res, url) => {
    const results = url.searchParams.getAll('r').map((target, index) =>
      `<li><h3><a class="result" href="${target.replaceAll('"', '&quot;')}">Result ${index + 1}</a></h3><p>A line of text about result ${index + 1}.</p></li>`);
    html(res, page(`Search: ${(url.searchParams.get('q') ?? '').replaceAll('<', '')}`, `<h1>Results</h1><p><a id="own" href="/search?q=more">More like this</a></p><ol id="results">${results.join('')}</ol>`));
  },

  // The input recorder (spec §10.4): what a page sees of the behavior engine.
  '/input/': (_req, res) => html(res, INPUT_PAGE),

  // The leave recorder (follow-up F28): how a page was left.
  '/leave/': (_req, res) => html(res, LEAVE_PAGE),

  // The activation recorder (spec §10.2): what a page is told of user
  // activation while an engine loads and reads it.
  '/activation/': (_req, res) => html(res, ACTIVATION_PAGE),

  '/__kage/requests': (_req, res) => json(res, requests),
  '/__kage/reports': (_req, res) => json(res, reports),
  '/__kage/reset': (_req, res) => {
    requests.length = 0;
    reports.length = 0;
    twice = 0;
    json(res, { ok: true });
  },
};

const server = http.createServer((req, res) => {
  const url = new URL(req.url ?? '/', `http://${req.headers.host ?? 'fixture.invalid'}`);
  if (url.pathname === '/__kage/report') {
    let body = '';
    req.on('data', (chunk) => (body += chunk));
    req.on('end', () => {
      try {
        reports.push(JSON.parse(body));
      } catch {
        reports.push({ unparsed: body.slice(0, 200) });
      }
      res.writeHead(204);
      res.end();
    });
    return;
  }
  if (!url.pathname.startsWith('/__kage/')) {
    requests.push({ method: req.method, host: req.headers.host ?? null, path: url.pathname + url.search, headers: headerPairs(req) });
  }
  const route = routes[url.pathname];
  if (route === undefined) {
    // The site a crawl reads (spec §13) lives in a file of its own.
    if (crawlRoute(req, res, url)) {
      return;
    }
    html(res, page('Not found', '<h1>Not found</h1>'), {}, 404);
    return;
  }
  route(req, res, url);
});

server.listen(port, host, () => {
  process.stdout.write(`${JSON.stringify({ fixture_site: true, host, port: server.address().port })}\n`);
});
for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => server.close(() => process.exit(0)));
}
