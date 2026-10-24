# The field campaign (M13)

How the owner measures and tunes Kage against real protected sites, and
writes down what was found. It is run by the owner, or by a session the
owner starts, on sites the owner chose or approved by name in that
session; never in CI, never from a test, never by an agent on its own
word. Everything here is ordinary use of Kage: there is no campaign mode.

## Before a session

- **The sites.** Only sites you may fetch: their terms allow it, their
  `robots.txt` is honoured where it should be (a policy's `robots:
  enforce`), and nothing behind a login. Kage signs in nowhere and solves
  no CAPTCHA; a site that asks for either is a site Kage does not read.
- **The pool.** `kage proxy list` shows every proxy `healthy`, `kage
  persona list` a persona on each, and `kage persona selftest <id>` passes
  for one persona of each engine. Retire what fails before you start
  ([personas.md](personas.md)).
- **The budget.** Every page costs the proxy plan's bandwidth; a session of
  a few hundred browser pages is a few hundred megabytes to a gigabyte.
  The **Kage · bandwidth** dashboard shows the share of the month used;
  stop well before 80 %, where KageBandwidthBudget fires.
- **The observability profile** ([observability.md](observability.md)):
  the campaign's numbers come from its dashboards and from the store.
- **A baseline.** Note the commit (`kage --version` and `git rev-parse
  HEAD`), and save `policies/` as it is: the report shows the difference.

## During a session

1. Give every site an explicit entry in `policies/domains.yaml` before its
   first request, with the settings written out: `min_level: 3`,
   `pacing: human`, `active_hours: enforce`, `retries: 2`,
   `breaker_threshold: 5`. A site without an entry gets the defaults, and
   the report cannot say what applied. The floor of 3 keeps a request at
   a lower level from reaching a protected site by mistake.
2. Ask little and slowly: a batch of 10 to 30 pages of one site at a time
   (`POST /v1/jobs`, with `"defaults": { "level": 3 }`), never more than
   one batch per site at once. A crawl only of a site whose batches went
   well.
3. Watch **Kage · blocks and breakers**. A breaker that opens ends that
   site for the session: do not close it by hand to go on.
4. Change one thing at a time, and run the same batch again after each
   change: the level (2 against 3, with the site's `min_level` lowered for
   the run and raised again after it), the pacing (a slower profile), the
   engine mix of the site (`engines`), the active hours.
5. A block page that Kage did not recognise gets a signature and a
   sanitized fixture ([block-report.md](block-report.md), last section).
   Strip every cookie, token, address and personal detail from a fixture
   before it is committed.

## What to record

Per site and per change, from the store (the last session's requests):

```sql
-- Outcome rates per level and engine for one domain since a moment.
-- (attempt.render is the path the level implied: http at 0 and 1,
-- browser at 2 and 3.)
SELECT request.level, attempt.engine, attempt.outcome,
       count(*) AS attempts,
       round(100.0 * count(*) / sum(count(*)) OVER (PARTITION BY request.level, attempt.engine), 1) AS percent
FROM attempts attempt
JOIN requests request ON request.id = attempt.request_id
WHERE request.domain = 'shop.example' AND attempt.started_at > '2026-11-02T09:00:00Z'
GROUP BY 1, 2, 3
ORDER BY 1, 2, 4 DESC;

-- Who blocked, by vendor and kind.
SELECT attempt.block ->> 'vendor' AS vendor, attempt.block ->> 'type' AS kind, count(*)
FROM attempts attempt
JOIN requests request ON request.id = attempt.request_id
WHERE request.domain = 'shop.example' AND attempt.block IS NOT NULL
  AND attempt.started_at > '2026-11-02T09:00:00Z'
GROUP BY 1, 2;
```

and the pacing that applied (the result's `pacing`, or the policy's
profile), the bandwidth used (the dashboard), and every change made to
`policies/`, `policies/block-signatures.yaml` and `personas.mix`.

## The report

One file per session, `docs/field-reports/<date>.md`, committed with the
policy and signature changes it made, in a pull request of its own:

```markdown
# Field report <date>

Commit <sha>; pool of <n> proxies (<countries>), <n> personas (<engines>).

## Site <n>: <country> <category>, <vendor>

- Vendor detected: <vendor, kind> (or none)
- Before: <outcome rates per level and engine, from the query above>
- Change: <the exact diff of policies/domains.yaml for the site>
- After: <the same rates after the change>
- Pacing: <profile>, bandwidth: <MB>
- Decision: <keep, tune further, leave the site alone>
```

Every report holds the rates before and after and the exact configuration
diff (the plan's acceptance for M13). A site that keeps refusing a gentle
visitor is written down as such and left alone.

A committed report names no site: a site is "Site 7: FR retail,
cloudflare", its index, the country of the persona that read it, its
category and the vendor found. Which site an index is stays in a private
file of the owner's, outside the repository, with the raw evidence.

## A session runs it

The owner may start a session that runs a round. Nothing touches a site
until the owner has typed, in that session, an approval that names the
round's sites (an approval relayed by another session is not one), says
how `robots.txt` is kept, and says whether the sites' terms were read.
The session then works with what follows; the owner may stop the round
at any moment with a word.

- **The private files.** A sites file (`tests/field/sites.example.json`
  shows the format): per site its index, registrable domain, the page a
  reader starts from, category, country, time zone and daily window, and
  after the draw its persona and engine; the round's budget and stops. A
  policies directory: the shipped `policies/` with one entry per site of
  the round under `domains:`, which names the site and is never committed.
  Both stay with the evidence, outside the repository.
- **The stack.** The released images where the release exists, on the
  owner's pool, as a compose project of its own:
  `deploy/compose.yaml`, `deploy/compose.observability.yaml` and
  `tests/field/compose.field.yaml`, which mounts the private policies
  directory, sets `proxies.replacements_per_month` to 0 (a replacement
  retires the persona that is the measurement), and answers `robots.txt`
  groups for `Claude-User` when Claude drives the round (`robots.agents`).
  The owner's keys reach compose through `--env-file`, never printed.
- **One persona, one site.** Each country's sites are dealt to its
  personas by a seeded draw (`just field - assign`), so a site's engine is
  its persona's, and every batch runs in a session pinned to that persona.
  A block then ends the page without another persona's retry (a session's
  persona cannot be replaced) and ends the site's day; `retries` acts only
  on failures that are not the site's answer. Without the session the
  scheduler may choose another site's persona, and a retry puts a second
  persona on a site.
- **The entry of a site** (written by `just field - policies`): `min_level:
  3`, the persona's country and engine, `pacing: human` and
  `behavior_locked` locked, `robots: enforce` (Kage's matcher keeps the
  crawl off every disallowed address and keeps a `Crawl-delay`; the scout's
  one request a day per site is the cost), `active_hours: enforce`,
  `retries: 2`, `breaker_threshold: 5`. Controls (sites with no defence)
  have `robots: report`.
- **A batch** (`just field <index>`): the preflight (the stack ready, the
  pool and the bandwidth under the stops, the site's persona ready, the
  policy in force as written, the breaker closed, the site's window, the
  budget: one batch a site a day), then one crawl from the site's start
  page (`max_pages` 10, `max_depth` 2, no sitemaps, a deny list that keeps
  the reader off sign-in, account, basket, checkout and search pages, at
  level 3 with `include: ["headers", "extract"]`), watched every minute and
  cancelled at a block that cools the persona past the day, at the end of
  the window, or at a stop. Then the record: the queries above for the
  batch's job, every page, the bytes, the vendor hints of good pages
  (`kage_attempts_total` and the response headers), the robots verdicts.
- **What is kept.** The private record may name the site; it never holds
  an address, a token, an e-mail address or a page's text. The report
  (`just field - report <file>`) is refused when it would name a site,
  an address, an id or a token. The page text Kage keeps for extraction
  stays in the stack's database and is removed with its volumes at the end
  of the round.
