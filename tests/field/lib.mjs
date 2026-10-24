// The pure part of the field campaign's runner (docs/runbooks/field-campaign.md,
// "A session runs it"): the private sites file, the requests the runner
// makes of Kage's API, the budget and the stops, the persona draw, the
// policy entries, the vendor hints, the scrubbing and the report section.
// Nothing here touches the network or the disk; tests/field/field.mjs does,
// and tests/field/lib.test.mjs tests this (node --test).
import { createHash } from 'node:crypto';

/** The engines a persona may run. */
export const ENGINES = ['chromium', 'firefox'];

/** What a field site's policy says unless its sites file says otherwise. */
export const DEFAULT_ENTRY = {
  min_level: 3,
  pacing: 'human',
  pacing_locked: true,
  behavior_locked: true,
  robots: 'enforce',
  active_hours: 'enforce',
  retries: 2,
  breaker_threshold: 5,
  breaker_pause: '30m',
  referer_strategy: 'direct',
  min_dwell: '2s',
  driver_input: false,
};

/** The budget of a round unless its sites file says otherwise. */
export const DEFAULT_BUDGET = {
  pages_per_day: 12,
  pages_per_site: 60,
  days_per_site: 3,
  pages_per_round: 600,
};

/** The stops of a round unless its sites file says otherwise. */
export const DEFAULT_STOPS = {
  min_usable_proxies: 16,
  max_term_share: 0.5,
  max_round_bytes: 3_000_000_000,
};

/**
 * What no batch of the round asks for, on any site: the pages behind which a
 * person signs in, buys, writes or searches. Case-insensitive, matched on the
 * normalized address by the crawl (`deny`, spec §13).
 */
export const COMMON_DENY = [
  '(?i)/(login|log-in|signin|sign-in|logon|anmelden|einloggen|connexion|accedi|accesso)([/?#._-]|$)',
  '(?i)/(register|registrieren|registrierung|signup|sign-up|inscription|registrati|registrazione)([/?#._-]|$)',
  '(?i)/(account|myaccount|my-account|konto|mein-konto|mon-compte|moncompte|profil|profile|mio-account|area-personale)([/?#._-]|$)',
  '(?i)/(cart|basket|warenkorb|panier|carrello|bag|checkout|kasse|commande|orderpipe|order|orders|bestellung)([/?#._-]|$)',
  '(?i)/(wishlist|wish-list|merkzettel|wunschliste|favoris|favoriti|favourites|favorites|saved)([/?#._-]|$)',
  '(?i)/(search|suche|recherche|ricerca|cerca|zoeken|szukaj)([/?#._-]|$)',
  '(?i)[?&](q|query|search|keyword|keywords|k|text|searchterm)=',
  '(?i)/(newsletter|contact|kontakt|contactez-nous|contatti|report|melden|signaler|segnala)([/?#._-]|$)',
  '(?i)/(logout|signout|sign-out|abmelden|deconnexion)([/?#._-]|$)',
  '(?i)/(password|passwort|mot-de-passe|reset)([/?#._-]|$)',
];

const HOST = /^(?=.{1,253}$)(?!-)[a-z0-9-]{1,63}(?<!-)(\.(?!-)[a-z0-9-]{1,63}(?<!-))+$/;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const CLOCK = /^([01]\d|2[0-3]):[0-5]\d$/;
const DURATION = /^\d+(ms|s|m|h|d)$/;

/** A problem with the private sites file, named by where it is. */
export class SitesError extends Error {}

/** The Rust-style inline flags of a deny pattern, so that JavaScript can check and use it. */
export function jsRegex(pattern) {
  const flags = /^\(\?([a-z]+)\)/.exec(pattern);
  const body = flags ? pattern.slice(flags[0].length) : pattern;
  return new RegExp(body, flags && flags[1].includes('i') ? 'i' : '');
}

/** Whether `host` is `domain` or one of its subdomains. */
export function hostMatches(host, domain) {
  const h = String(host).toLowerCase().replace(/\.$/, '');
  return h === domain || h.endsWith(`.${domain}`);
}

/**
 * Reads and checks the private sites file. Throws a SitesError naming the
 * first field that is wrong; returns the round with its defaults filled in.
 */
export function parseSites(text) {
  let round;
  try {
    round = JSON.parse(text);
  } catch (error) {
    throw new SitesError(`the sites file is no JSON: ${error.message}`);
  }
  const fail = (where, what) => {
    throw new SitesError(`${where}: ${what}`);
  };
  if (typeof round !== 'object' || round === null) fail('the file', 'an object is expected');
  if (typeof round.round !== 'string' || !/^[a-z0-9-]{1,40}$/.test(round.round)) {
    fail('round', 'a name of 1 to 40 lowercase letters, digits and hyphens');
  }
  if (typeof round.draw_seed !== 'string' || round.draw_seed.length < 8) {
    fail('draw_seed', 'a seed of at least 8 characters');
  }
  if (Number.isNaN(Date.parse(round.since ?? ''))) fail('since', 'an RFC 3339 time');
  const budget = { ...DEFAULT_BUDGET, ...(round.budget ?? {}) };
  for (const [name, value] of Object.entries(budget)) {
    if (!Number.isInteger(value) || value < 1) fail(`budget.${name}`, 'a positive whole number');
  }
  if (budget.pages_per_day > DEFAULT_BUDGET.pages_per_day) {
    fail('budget.pages_per_day', `at most ${DEFAULT_BUDGET.pages_per_day}`);
  }
  const stops = { ...DEFAULT_STOPS, ...(round.stops ?? {}) };
  if (!(stops.max_term_share > 0 && stops.max_term_share <= 1)) fail('stops.max_term_share', 'a share above 0 and at most 1');
  if (!Number.isInteger(stops.min_usable_proxies) || stops.min_usable_proxies < 1) {
    fail('stops.min_usable_proxies', 'a positive whole number');
  }
  if (!Number.isInteger(stops.max_round_bytes) || stops.max_round_bytes < 1) fail('stops.max_round_bytes', 'bytes');
  const deny = round.deny ?? COMMON_DENY;
  checkPatterns(deny, 'deny', fail);
  const warmupHosts = round.warmup_hosts ?? [];
  if (!Array.isArray(warmupHosts) || warmupHosts.some((host) => typeof host !== 'string' || !HOST.test(host))) {
    fail('warmup_hosts', 'a list of lowercase host names');
  }
  if (!Array.isArray(round.sites) || round.sites.length === 0) fail('sites', 'a list of at least one site');
  const indexes = new Set();
  const personas = new Map();
  const sites = round.sites.map((site, n) => {
    const where = `sites[${n}]`;
    if (typeof site.index !== 'string' || !/^(C?\d{1,2})$/.test(site.index)) {
      fail(`${where}.index`, 'a number or C and a number, as text ("7", "C1")');
    }
    if (indexes.has(site.index)) fail(`${where}.index`, `${site.index} twice`);
    indexes.add(site.index);
    if (typeof site.domain !== 'string' || !HOST.test(site.domain)) fail(`${where}.domain`, 'a lowercase host name');
    let seed;
    try {
      seed = new URL(site.seed);
    } catch {
      fail(`${where}.seed`, 'an absolute address');
    }
    if (!['https:', 'http:'].includes(seed.protocol) || !hostMatches(seed.hostname, site.domain)) {
      fail(`${where}.seed`, `an http(s) address on ${site.domain}`);
    }
    if (typeof site.category !== 'string' || !/^[a-z][a-z ,:-]{1,60}$/.test(site.category)) {
      fail(`${where}.category`, 'a few lowercase words');
    }
    if (typeof site.country !== 'string' || !/^[A-Z]{2}$/.test(site.country)) fail(`${where}.country`, 'a two-letter country');
    if (site.engine !== null && site.engine !== undefined && !ENGINES.includes(site.engine)) {
      fail(`${where}.engine`, `one of ${ENGINES.join(', ')}, or null before the draw`);
    }
    if (site.persona !== null && site.persona !== undefined) {
      if (typeof site.persona !== 'string' || !UUID.test(site.persona)) fail(`${where}.persona`, "a persona's id, or null before the draw");
      if (!site.control && personas.has(site.persona)) {
        fail(`${where}.persona`, `already the reader of site ${personas.get(site.persona)}: one persona reads one site`);
      }
      if (!site.control) personas.set(site.persona, site.index);
    }
    if (typeof site.timezone !== 'string' || !validZone(site.timezone)) fail(`${where}.timezone`, 'an IANA time zone');
    const window = site.window ?? { from: '09:30', until: '20:00' };
    if (!CLOCK.test(window.from ?? '') || !CLOCK.test(window.until ?? '') || window.from >= window.until) {
      fail(`${where}.window`, 'from and until as HH:MM, from before until');
    }
    const pages = site.pages ?? 10;
    if (!Number.isInteger(pages) || pages < 1 || pages > budget.pages_per_day) {
      fail(`${where}.pages`, `1 to ${budget.pages_per_day}`);
    }
    const maxDepth = site.max_depth ?? 2;
    if (!Number.isInteger(maxDepth) || maxDepth < 0 || maxDepth > 32) fail(`${where}.max_depth`, '0 to 32');
    const names = site.names ?? [];
    if (!Array.isArray(names) || names.some((name) => typeof name !== 'string' || name.length < 3)) {
      fail(`${where}.names`, 'strings of at least 3 characters');
    }
    const siteDeny = site.deny ?? [];
    checkPatterns(siteDeny, `${where}.deny`, fail);
    if (deny.length + siteDeny.length > 32) fail(`${where}.deny`, 'a crawl takes at most 32 patterns with the common ones');
    const entry = { ...DEFAULT_ENTRY, ...(site.control ? { robots: 'report' } : {}), ...(site.entry ?? {}) };
    if (!['ignore', 'report', 'enforce'].includes(entry.robots)) fail(`${where}.entry.robots`, 'ignore, report or enforce');
    if (!['prefer', 'enforce', 'ignore'].includes(entry.active_hours)) fail(`${where}.entry.active_hours`, 'prefer, enforce or ignore');
    if (!DURATION.test(entry.breaker_pause) || !DURATION.test(entry.min_dwell)) fail(`${where}.entry`, 'durations as 30m, 2s');
    if (entry.min_level !== 3) fail(`${where}.entry.min_level`, 'every page of the round is read at level 3');
    return {
      ...site,
      control: Boolean(site.control),
      engine: site.engine ?? null,
      persona: site.persona ?? null,
      window,
      pages,
      max_depth: maxDepth,
      names,
      deny: siteDeny,
      entry,
    };
  });
  return { ...round, budget, stops, deny, warmup_hosts: warmupHosts, sites };
}

/**
 * The breaker threshold of a warm-up host during a round. Until follow-up
 * F85, Kage judges every warm-up visit a soft block (the visit captures no
 * document), so a warm-up host's breaker would open on visits that were
 * fine and the persona's later visits there would be skipped; a threshold
 * the round cannot reach keeps the warm-up what it is meant to be.
 */
export const WARMUP_BREAKER_THRESHOLD = 1000;

function checkPatterns(list, where, fail) {
  if (!Array.isArray(list)) fail(where, 'a list of patterns');
  list.forEach((pattern, n) => {
    if (typeof pattern !== 'string' || pattern.length === 0 || pattern.length > 512) {
      fail(`${where}[${n}]`, 'a pattern of 1 to 512 bytes');
    }
    try {
      jsRegex(pattern);
    } catch (error) {
      fail(`${where}[${n}]`, `not a regular expression: ${error.message}`);
    }
  });
}

function validZone(zone) {
  try {
    new Intl.DateTimeFormat('en-GB', { timeZone: zone });
    return true;
  } catch {
    return false;
  }
}

/** The site of a round by its index; throws when it has none. */
export function siteOf(round, index) {
  const site = round.sites.find((one) => one.index === String(index));
  if (!site) throw new SitesError(`no site has the index ${index}`);
  return site;
}

/** The local date (YYYY-MM-DD) and clock (HH:MM) of a moment in a time zone. */
export function localTime(at, zone) {
  const parts = Object.fromEntries(
    new Intl.DateTimeFormat('en-CA', {
      timeZone: zone,
      year: 'numeric',
      month: '2-digit',
      day: '2-digit',
      hour: '2-digit',
      minute: '2-digit',
      hourCycle: 'h23',
    })
      .formatToParts(at)
      .map((part) => [part.type, part.value]),
  );
  return { date: `${parts.year}-${parts.month}-${parts.day}`, clock: `${parts.hour}:${parts.minute}` };
}

/** Whether a moment lies inside the site's daily window, in the site's own time. */
export function inWindow(site, at) {
  const { clock } = localTime(at, site.timezone);
  return clock >= site.window.from && clock < site.window.until;
}

/**
 * The moment the site's window ends on the local day of `at`: found by
 * walking forward a minute at a time (time zones have no arithmetic of
 * their own here), at most a day.
 */
export function windowEnd(site, at) {
  const start = Math.floor(at.getTime() / 60_000) * 60_000;
  const day = localTime(at, site.timezone).date;
  for (let minute = 0; minute <= 24 * 60; minute += 1) {
    const moment = new Date(start + minute * 60_000);
    const local = localTime(moment, site.timezone);
    if (local.date !== day || local.clock >= site.window.until) return moment;
  }
  return new Date(start + 24 * 3_600_000);
}

/**
 * How many pages the site's batch may ask for now, or why none: `counts`
 * are the pages the round's key asked of the site today (in the site's own
 * time), in all, on how many days, and of the whole round. A site has one
 * batch a day.
 */
export function pagesAllowed(site, budget, counts) {
  if (counts.today > 0) return { pages: 0, reason: 'the site had its batch today' };
  if (counts.days >= budget.days_per_site) return { pages: 0, reason: `the site had its ${budget.days_per_site} days` };
  if (counts.site >= budget.pages_per_site) return { pages: 0, reason: `the site had its ${budget.pages_per_site} pages` };
  if (counts.round >= budget.pages_per_round) return { pages: 0, reason: `the round had its ${budget.pages_per_round} pages` };
  const pages = Math.min(
    site.pages,
    budget.pages_per_day,
    budget.pages_per_site - counts.site,
    budget.pages_per_round - counts.round,
  );
  return { pages, reason: null };
}

/**
 * Why the round must not start or go on, from the admin API's pool view
 * and the round's own bytes; null when nothing stops it.
 */
export function stopReason(pool, roundBytes, stops) {
  const proxies = pool?.proxies ?? {};
  const usable = Number(proxies.usable ?? 0);
  if (proxies.egress_unsafe === true) return 'a leak is on record';
  if (usable < stops.min_usable_proxies) return `${usable} proxies are usable, fewer than ${stops.min_usable_proxies}`;
  const bandwidth = pool?.bandwidth;
  if (bandwidth?.stop === true) return "the plan's bandwidth stop is in force";
  const share = Number(bandwidth?.share ?? 0);
  if (share >= stops.max_term_share) {
    return `the term's bandwidth share is ${(share * 100).toFixed(1)} %, at or over ${stops.max_term_share * 100} %`;
  }
  if (roundBytes >= stops.max_round_bytes) {
    return `the round used ${megabytes(roundBytes)} MB, at or over ${megabytes(stops.max_round_bytes)} MB`;
  }
  return null;
}

/** A small seeded generator (mulberry32 over the first word of a SHA-256). */
export function seeded(seed) {
  let state = createHash('sha256').update(seed).digest().readUInt32LE(0);
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** The items in an order drawn from `seed` (Fisher-Yates). */
export function shuffle(items, seed) {
  const next = seeded(seed);
  const out = items.slice();
  for (let i = out.length - 1; i > 0; i -= 1) {
    const j = Math.floor(next() * (i + 1));
    [out[i], out[j]] = [out[j], out[i]];
  }
  return out;
}

/**
 * Deals each country's sites to that country's personas by a draw seeded by
 * the round's seed and the country: sites in index order, personas in the
 * order of their ids, then shuffled. A site takes its persona's engine.
 * Controls are not dealt (they are read by a persona of the day). Throws
 * when a country has fewer live personas than sites.
 */
export function dealPersonas(round, personas) {
  const live = personas.filter((persona) => persona.state !== 'retired');
  const countries = [...new Set(round.sites.filter((site) => !site.control).map((site) => site.country))].sort();
  const dealt = [];
  for (const country of countries) {
    const sites = round.sites
      .filter((site) => !site.control && site.country === country)
      .sort((a, b) => Number(a.index) - Number(b.index));
    const mine = live.filter((persona) => persona.country === country).sort((a, b) => (a.id < b.id ? -1 : 1));
    if (mine.length < sites.length) {
      throw new SitesError(`${country} has ${sites.length} sites and ${mine.length} live personas`);
    }
    const order = shuffle(mine, `${round.draw_seed}:${country}`);
    sites.forEach((site, n) => {
      dealt.push({
        index: site.index,
        country,
        persona: order[n].id,
        engine: order[n].engine,
        claimed_os: order[n].claimed_os,
      });
    });
  }
  return dealt.sort((a, b) => Number(a.index) - Number(b.index));
}

/** The registrable domain of a site written as an anonymous key: `site-7.example`. */
export function anonymousKey(site) {
  return `site-${site.index.toLowerCase()}.example`;
}

/** One site's entry of `domains.yaml`, indented under `domains:`. */
export function policyEntry(site, { key = site.domain, comment = `site ${site.index}` } = {}) {
  if (!site.engine && !site.control) throw new SitesError(`site ${site.index} has no engine yet: draw the personas first`);
  const entry = site.entry;
  const lines = [`  "${key}": # ${comment}`, `    min_level: ${entry.min_level}`];
  if (!site.control) {
    lines.push(`    countries: [${site.country}]`, `    engines: [${site.engine}]`);
  }
  lines.push(
    `    pacing: ${entry.pacing}`,
    `    pacing_locked: ${entry.pacing_locked}`,
    `    behavior_locked: ${entry.behavior_locked}`,
    `    robots: ${entry.robots}`,
    `    active_hours: ${entry.active_hours}`,
    `    retries: ${entry.retries}`,
    `    breaker_threshold: ${entry.breaker_threshold}`,
    `    breaker_pause: ${entry.breaker_pause}`,
    `    referer_strategy: ${entry.referer_strategy}`,
    `    min_dwell: ${entry.min_dwell}`,
    `    driver_input: ${entry.driver_input}`,
  );
  return `${lines.join('\n')}\n`;
}

/**
 * The shipped `domains.yaml` with the round's entries added to its
 * `domains:` mapping, which must be the file's last key.
 */
export function domainsFile(shipped, round) {
  const text = shipped.replace(/\n*$/, '\n').replace(/^domains: \{\}$/m, 'domains:');
  const keys = text.split('\n').filter((line) => /^[a-z_]+:/.test(line));
  if (keys.at(-1) !== 'domains:') throw new SitesError("the shipped domains.yaml does not end with its domains mapping");
  const entries = round.sites.map((site) => policyEntry(site)).join('');
  const warmup = round.warmup_hosts
    .map((host) => `  "${host}": { breaker_threshold: ${WARMUP_BREAKER_THRESHOLD} } # a warm-up host (F85)\n`)
    .join('');
  return `${text}  # The field campaign's round ${round.round} (tests/field), never committed.\n${entries}${warmup}`;
}

/** What `GET /v1/policies/domains/{host}` must show for the site, as [field, expected, found]. */
export function policyDifferences(site, view) {
  const policy = view?.policy ?? view ?? {};
  const expected = {
    min_level: site.entry.min_level,
    pacing: site.entry.pacing,
    pacing_locked: site.entry.pacing_locked,
    behavior_locked: site.entry.behavior_locked,
    robots: site.entry.robots,
    active_hours: site.entry.active_hours,
    retries: site.entry.retries,
    breaker_threshold: site.entry.breaker_threshold,
  };
  if (!site.control) {
    expected.countries = [site.country];
    expected.engines = [site.engine];
  }
  const differences = [];
  for (const [field, want] of Object.entries(expected)) {
    const found = policy[field] ?? view?.[field];
    if (JSON.stringify(found) !== JSON.stringify(want)) differences.push([field, want, found]);
  }
  return differences;
}

/**
 * The session ids a site's batch of a day tries, in order. Among personas
 * with as few open sessions, the session's id decides (spec §5), so with
 * three such personas 24 ids miss the site's persona once in 17 000 days.
 */
export function sessionIds(round, site, date, tries = 24) {
  const day = date.replaceAll('-', '');
  return Array.from({ length: tries }, (_, n) => `${round.round}-s${site.index.toLowerCase()}-${day}-${n}`);
}

/**
 * The body of `POST /v1/sessions` for a site's batch: its persona's country
 * and engine as conditions (a control's are its reader's of the day).
 */
export function sessionRequest(site, id) {
  const host = new URL(site.seed).hostname;
  if (!site.engine) throw new SitesError(`site ${site.index} has no engine for its session`);
  return { id, domain: host, persona: { countries: [site.country], engines: [site.engine] } };
}

/** The body of `POST /v1/jobs` for a site's batch of a day. */
export function crawlRequest(round, site, { sessionId, pages, include = ['headers', 'extract'] }) {
  return {
    type: 'crawl',
    seeds: [site.seed],
    max_pages: pages,
    max_depth: site.max_depth,
    same_site_only: true,
    use_sitemaps: false,
    session_id: sessionId,
    deny: [...round.deny, ...site.deny],
    defaults: {
      level: 3,
      include,
      ...(include.includes('extract') ? { extract: { markdown: false, links: false } } : {}),
    },
  };
}

/** The outcomes that are the site's answer, not a page (spec §8.3). */
export const BLOCKS = ['rate_limited', 'forbidden', 'captcha', 'challenge_failed', 'soft_block'];

const COOLDOWN_MS = {
  rate_limited: 2 * 3_600_000,
  forbidden: 24 * 3_600_000,
  captcha: 24 * 3_600_000,
  challenge_failed: 24 * 3_600_000,
  soft_block: 6 * 3_600_000,
  short: 30 * 60_000,
};

/**
 * What a block means for the day: the persona's cooldown (the policy's
 * default for the outcome, `short` for a weak attribution, scaled by the
 * confidence, at least five minutes) and whether it outlasts the site's
 * window, which ends the batch.
 */
export function afterBlock(result, now, end) {
  const block = result.block ?? {};
  const weak = (block.vendor ?? 'generic') === 'generic' && Number(block.confidence ?? 0) < 0.9;
  const base = weak ? COOLDOWN_MS.short : COOLDOWN_MS[result.outcome] ?? COOLDOWN_MS.forbidden;
  const cooldown = Math.max(5 * 60_000, Math.round(base * Number(block.confidence ?? 1)));
  const until = new Date(now.getTime() + cooldown);
  return { cooldown_ms: cooldown, until, ends_day: until.getTime() >= end.getTime() };
}

const VENDOR_COOKIES = [
  ['cloudflare', /^(__cf_bm|cf_clearance|__cflb|_cfuvid|cf_chl_.*)$/i],
  ['akamai', /^(_abck|bm_sz|ak_bmsc|bm_sv|bm_mi|bm_so|bm_lso|bm_sc|sbsd.*|akacd_.*|akavpau_.*|akaas_.*)$/i],
  ['datadome', /^datadome$/i],
  ['human', /^(_px.*|_pxhd|pxcts)$/i],
  ['kasada', /^(kp_uidz|x-kpsdk.*)$/i],
  ['imperva', /^(incap_ses_.*|visid_incap_.*|nlbi_.*|reese84)$/i],
  ['aws_waf', /^aws-waf-token$/i],
  ['f5_shape', /^TS0[0-9a-f]{6,}$/i],
  ['queue_it', /^QueueIT.*/i],
];

const VENDOR_HEADERS = [
  ['cloudflare', (name, value) => name === 'cf-ray' || name === 'cf-mitigated' || (name === 'server' && /^cloudflare/i.test(value))],
  ['akamai', (name, value) => name.startsWith('akamai-') || name === 'x-akamai-transformed' || (name === 'server' && /^Akamai/i.test(value))],
  ['datadome', (name) => name === 'x-datadome' || name === 'x-datadome-cid' || name === 'x-dd-b'],
  ['human', (name) => name.startsWith('x-px-')],
  ['kasada', (name) => name.startsWith('x-kpsdk-')],
  ['imperva', (name, value) => name === 'x-iinfo' || (name === 'x-cdn' && /imperva|incapsula/i.test(value))],
  ['aws_waf', (name) => name.startsWith('x-amzn-waf')],
];

/**
 * The vendors a page's response headers name (header names, the server
 * header, and the names of the cookies it set; Kage redacts the values),
 * sorted.
 */
export function vendorHints(headers) {
  const found = new Set();
  for (const header of headers ?? []) {
    const name = String(header.name ?? '').toLowerCase();
    const value = String(header.value ?? '');
    for (const [vendor, test] of VENDOR_HEADERS) if (test(name, value)) found.add(vendor);
    if (name === 'set-cookie') {
      const cookie = value.split('=')[0].trim();
      for (const [vendor, pattern] of VENDOR_COOKIES) if (pattern.test(cookie)) found.add(vendor);
    }
  }
  return [...found].sort();
}

/** The names of the cookies a page's response set (never their values). */
export function cookieNames(headers) {
  return [
    ...new Set(
      (headers ?? [])
        .filter((header) => String(header.name).toLowerCase() === 'set-cookie')
        .map((header) => String(header.value).split('=')[0].trim())
        .filter(Boolean),
    ),
  ].sort();
}

/**
 * The samples of `kage_attempts_total` for the site's hosts in a /metrics
 * exposition: `{ level, outcome, vendor, count }`, summed over the hosts.
 */
export function attemptsFromMetrics(text, domain) {
  const sums = new Map();
  for (const line of String(text).split('\n')) {
    if (!line.startsWith('kage_attempts_total{')) continue;
    const match = /^kage_attempts_total\{([^}]*)\}\s+([0-9.e+-]+)/.exec(line);
    if (!match) continue;
    const labels = Object.fromEntries(
      [...match[1].matchAll(/([a-z_]+)="((?:[^"\\]|\\.)*)"/g)].map((label) => [label[1], label[2]]),
    );
    if (!labels.domain || !hostMatches(labels.domain, domain)) continue;
    const key = `${labels.level}|${labels.outcome}|${labels.vendor ?? ''}`;
    sums.set(key, (sums.get(key) ?? 0) + Number(match[2]));
  }
  return [...sums]
    .map(([key, count]) => {
      const [level, outcome, vendor] = key.split('|');
      return { level, outcome, vendor, count };
    })
    .sort((a, b) => b.count - a.count || (a.outcome < b.outcome ? -1 : 1));
}

const IPV4 = /\b(?:(?:25[0-5]|2[0-4]\d|1?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|1?\d?\d)\b/g;
const IPV6 = /\b(?:[0-9a-f]{1,4}:){3,7}[0-9a-f]{1,4}\b|\b(?:[0-9a-f]{1,4}:){1,6}:(?:[0-9a-f]{1,4}:?){1,6}\b/gi;
const EMAIL = /\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b/g;
const PHONE = /(?<![\w.-])\+?\d[\d ()./-]{8,}\d(?![\w-])/g;
const QUERY_TOKEN = /([?&;](?:token|key|sig|signature|auth|session|sid|jsessionid|code|ticket|hash)[^=&]*=)[^&#\s"']+/gi;

/**
 * Private evidence without the addresses, e-mail addresses, phone numbers
 * and query-string tokens a page or a header may carry, deep through
 * objects and lists. What it keeps may still name the site: that stays in
 * the private directory.
 */
export function scrubPrivate(value) {
  if (typeof value === 'string') {
    return value
      .replace(EMAIL, '[email]')
      .replace(QUERY_TOKEN, '$1[token]')
      .replace(IPV4, '[address]')
      .replace(IPV6, (match) => (/[a-f]/i.test(match) || match.split(':').length > 4 ? '[address]' : match))
      .replace(PHONE, (match) => (match.replace(/\D/g, '').length >= 9 ? '[phone]' : match));
  }
  if (Array.isArray(value)) return value.map(scrubPrivate);
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).map(([key, inner]) => [key, scrubPrivate(inner)]));
  }
  return value;
}

/**
 * Why a text meant for the repository may not be committed: every name of
 * a site of the round (its domain, the domain's first label, the hosts it
 * redirected to, the names the sites file lists), an IPv4 or IPv6 literal,
 * an e-mail address, a persona id or handle, a long token. Empty when it is
 * clean.
 */
export function leaks(text, round, extra = []) {
  const found = [];
  const lower = String(text).toLowerCase();
  const names = new Set(extra.map((name) => name.toLowerCase()));
  for (const site of round.sites) {
    names.add(site.domain);
    names.add(site.domain.split('.')[0]);
    for (const name of site.names) names.add(name.toLowerCase());
  }
  // `example` is the reserved name the report's own keys use (RFC 2606).
  names.delete('example');
  for (const name of names) {
    if (name.length < 3) continue;
    const pattern = new RegExp(`(^|[^a-z0-9])${name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}([^a-z0-9]|$)`);
    if (pattern.test(lower)) found.push(`the name ${JSON.stringify(name)}`);
  }
  if (new RegExp(IPV4.source).test(text)) found.push('an IPv4 address');
  if (/\b(?:[0-9a-f]{1,4}:){3,7}[0-9a-f]{1,4}\b/i.test(text)) found.push('an IPv6 address');
  if (new RegExp(EMAIL.source).test(text)) found.push('an e-mail address');
  if (/\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/i.test(text)) found.push("an id (a persona's or a request's)");
  if (/\bp_[0-9a-f]{16}\b/.test(text)) found.push('a persona handle');
  const withoutIds = String(text).replace(/\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/gi, '');
  if (/\b(?=[A-Za-z0-9_-]*\d)(?=[A-Za-z0-9_-]*[A-Za-z])[A-Za-z0-9_-]{32,}\b/.test(withoutIds)) found.push('a long token');
  if (/https?:\/\/(?!site-[0-9a-z]+\.example)(?!shop\.example)[^\s)]+/i.test(text)) found.push('an address of a page');
  for (const time of String(text).matchAll(/\d{4}-\d\d-\d\dT\d\d:(\d\d)(?::(\d\d)(?:\.\d+)?)?/g)) {
    if (time[1] !== '00' || (time[2] ?? '00') !== '00') {
      found.push('a time finer than the hour');
      break;
    }
  }
  return found;
}

/** Megabytes with one decimal. */
export function megabytes(bytes) {
  return (Number(bytes) / 1_000_000).toFixed(1);
}

/** The median of numbers (the lower middle of an even list); null when empty. */
export function median(numbers) {
  const sorted = numbers.filter((n) => Number.isFinite(n)).sort((a, b) => a - b);
  if (sorted.length === 0) return null;
  return sorted[Math.floor((sorted.length - 1) / 2)];
}

/** How many of each value a list holds, as "value n" pairs in descending order. */
export function tally(values) {
  const counts = new Map();
  for (const value of values) counts.set(value, (counts.get(value) ?? 0) + 1);
  return [...counts].sort((a, b) => b[1] - a[1] || String(a[0]).localeCompare(String(b[0])));
}

/** The vendor a record names for its site: the one blocks named, else the hints most pages carried. */
export function vendorOf(record) {
  const blocked = record.blocks.map((block) => block.vendor).filter((vendor) => vendor && vendor !== 'generic');
  if (blocked.length > 0) return tally(blocked)[0][0];
  const hinted = record.pages.flatMap((page) => page.vendors);
  const kage = record.metrics.map((row) => row.vendor);
  const all = [...hinted, ...kage].filter((vendor) => vendor && !['none', 'generic', 'queue_it'].includes(vendor));
  return all.length > 0 ? tally(all)[0][0] : 'none seen';
}

const ENGINE_NAMES = { chromium: 'Chrome', firefox: 'Firefox' };
const OS_NAMES = { linux: 'Linux', windows: 'Windows', macos: 'macOS' };

/** The heading of a site's section: `Site 7: FR retail, cloudflare`. */
export function heading(site, vendor) {
  return `Site ${site.index}: ${site.country} ${site.category}, ${vendor}`;
}

/** A table of the runbook's first query (rates per level and engine). */
export function ratesTable(rows) {
  if (rows.length === 0) return '_no attempt_';
  const lines = ['| Level | Engine | Outcome | Attempts | % |', '|---|---|---|---|---|'];
  for (const row of rows) {
    lines.push(`| ${row.level} | ${row.engine} | ${row.outcome} | ${row.attempts} | ${Number(row.percent).toFixed(1)} |`);
  }
  return lines.join('\n');
}

/**
 * A site's section of the report from its records, the first pass's and,
 * when there was one, the second's. Holds no name of the site, no address,
 * no id: `leaks` is the last check before it is written.
 */
export function renderSection(site, passes) {
  const [before, after] = passes;
  const vendor = vendorOf(after ?? before);
  const persona = before.persona;
  const lines = [`## ${heading(site, vendor)}`, ''];
  lines.push(
    `- Persona: one ${ENGINE_NAMES[persona.engine] ?? persona.engine} persona in ${persona.country} claiming ${OS_NAMES[persona.claimed_os] ?? persona.claimed_os}, ${site.control ? 'a reader of a site of the round that day (a control is none)' : "the site's only reader"}.`,
  );
  const hinted = tally(before.pages.flatMap((page) => page.vendors));
  const blocks = tally(before.blocks.map((block) => `${block.vendor} ${block.type}`));
  lines.push(
    `- Vendor detected: ${vendor}; hints on the pages: ${hinted.length ? hinted.map(([name, n]) => `${name} ${n}`).join(', ') : 'none'}; blocks: ${blocks.length ? blocks.map(([name, n]) => `${name} ${n}`).join(', ') : 'none'}.`,
  );
  lines.push('', `**Before** (pass 1, ${before.date}):`, '', ratesTable(before.rates), '');
  lines.push(`- ${summary(before)}`);
  if (after) {
    lines.push('', `- Change: ${after.change}`, '', `**After** (pass 2, ${after.date}):`, '', ratesTable(after.rates), '');
    lines.push(`- ${summary(after)}`);
  } else {
    lines.push('- Change: none.', '- After: none (no second pass).');
  }
  const pacing = after ?? before;
  lines.push(
    `- Pacing: ${pacing.pacing}; bandwidth: ${passes.map((pass) => `${megabytes(pass.bytes)} MB (${pass.pages.length ? megabytes(pass.bytes / pass.pages.length) : '0.0'} MB a page)`).join(', then ')}.`,
  );
  lines.push(`- Decision: ${(after ?? before).decision}.`, '');
  return lines.join('\n');
}

/**
 * The report's warm-up section (follow-up F85): per site, how many of its
 * persona's warm-up visits were made and how many skipped, and whether the
 * persona was warmed before its first batch. Kage's verdicts on warm-up
 * visits are not shown: until F85 every one of them is a soft block.
 * `rows` are the private warm-up record's, `firstBatch` maps a site's
 * index to the start of its first batch.
 */
export function renderWarmup(round, rows, firstBatch = {}) {
  const hour = (at) => (at ? `${String(at).slice(0, 13)}:00Z` : '-');
  const lines = [
    '## Warm-up (F85)',
    '',
    "Kage's verdicts on this round's warm-up visits mean nothing: a warm-up visit captures no document, so Kage judged every one a soft block (follow-up F85). The visits themselves were made as designed, through each persona's own browser with the behaviour engine. What counts is how many were made and how many were skipped (a visit is skipped when the persona may not go there, and counted toward `warmed_at` all the same), and whether a persona was warmed before its first batch.",
    '',
    '| Site | Engine | Country | Visits made | Skipped | Warmed at | Warmed before its first batch |',
    '|---|---|---|---|---|---|---|',
  ];
  const sites = round.sites.filter((site) => !site.control && site.persona).sort((a, b) => Number(a.index) - Number(b.index));
  for (const site of sites) {
    const row = rows.find((one) => one.id === site.persona);
    if (!row) continue;
    const first = firstBatch[site.index];
    const before = first && row.warmed_at ? (Date.parse(row.warmed_at) <= Date.parse(first) ? 'yes' : 'no') : '-';
    lines.push(`| ${site.index} | ${row.engine} | ${row.country} | ${row.made} | ${row.skipped} | ${hour(row.warmed_at)} | ${before} |`);
  }
  const made = rows.reduce((sum, row) => sum + Number(row.made), 0);
  const skipped = rows.reduce((sum, row) => sum + Number(row.skipped), 0);
  lines.push('', `In all ${made} warm-up visits made and ${skipped} skipped by ${rows.length} personas; none of them is counted in the round's 600 page loads.`, '');
  return lines.join('\n');
}

function summary(record) {
  const read = record.pages.filter((page) => ['ok', 'challenge_passed'].includes(page.outcome)).length;
  const reached = tally(record.pages.map((page) => page.reached ?? 'not recorded'))
    .map(([name, n]) => `${name} ${n}`)
    .join(', ');
  const ends = tally(record.pages.map((page) => page.scroll_ended_by).filter(Boolean))
    .map(([name, n]) => `${name} ${n}`)
    .join(', ');
  const seconds = median(record.pages.map((page) => page.total_ms));
  const words = median(record.pages.filter((page) => page.word_count !== null).map((page) => page.word_count));
  const skipped = Object.entries(record.skipped ?? {})
    .filter(([, n]) => n > 0)
    .map(([name, n]) => `${name} ${n}`)
    .join(', ');
  const unread = record.unread?.length ?? 0;
  return [
    `${record.pages.length} pages asked, ${read} read${unread ? `, ${unread} left unread when the batch ended` : ''}`,
    `reached: ${reached || 'none'}`,
    `a page took ${seconds === null ? '-' : (seconds / 1000).toFixed(1)} s at the median`,
    `scrolling ended by ${ends || '-'}`,
    `median words ${words ?? '-'}`,
    `skipped: ${skipped || 'none'}`,
    `robots.txt: ${record.robots}`,
    // The record keeps the moment; the report says the hour.
    `ended: ${String(record.ended).replace(/(\d{4}-\d\d-\d\dT\d\d):\d\d(?::\d\d(?:\.\d+)?)?Z/g, '$1:00Z')}`,
  ].join('; ') + '.';
}
