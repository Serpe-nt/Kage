# ADR 0012: Camouflage levels, chosen on every request, and the operator's choice of egress

Status: accepted, 2026-10-19, by the owner (the question round of the
2.0 phase, recorded in `docs/00-decisions.md` D50). Changes the
non-negotiable "never add a direct (non-proxy) connection path to
targets" of `CLAUDE.md` and the floors of D40 for the levels below 3.
Ships in 2.0.0 (M14).

## Context

1.0 has one way of fetching: a persona on a residential proxy, a real
browser or an emulated http client, human pacing, behaviour floors a
caller cannot go under (D40), and a rule that no request ever leaves
the host's own address. That is right for sites that watch, and wrong
for the owner's other cases: a page that can be read with a plain HTTP
client, a site the operator is allowed to load fast, and an operator who
runs Kage on their own machine and is content for it to use their own
connection. The owner asked for several levels of "how much this looks
like a real user", chosen by the caller on every request, with no
persona at the lowest level, and for proxies to be optional.

The stealth promise of §1 ("indistinguishable at every layer") cannot
hold at the lower levels, and it is not meant to: the level is the
caller saying how much of it they want for this request, and a site's
policy is the operator saying how little they will accept for that site.

## Decision

1. **Every request names a `level`, 0 to 3; a request without one is
   refused** (`level_required`). The levels:
   - **0, plain**: an HTTP client and nothing else. No persona, no
     browser, no behaviour, no pacing of a persona's; wreq with its own
     TLS, a `User-Agent` of `kage/<version>` unless the caller sets one,
     the caller's headers and body as given, no cookie jar (a `Cookie`
     header is the caller's). Egress is whatever `egress.plain` says:
     the host's own address, or, under the pool, one address per site
     and never the pool in turn: one of the pool's own plain exits (a
     declared entry without a country, ADR 0013), chosen once per site
     and kept, else the address of the site's scout (the persona that
     asks the site for what no reader asks for and never reads it). M14
     found that rotating level 0 through the pool would show a site the
     personas' own addresses sending `kage/<version>`; an address that
     does so must never be one that reads the site as a person.
     `plain_exit_unavailable` when the site's exit is not usable, never
     another address.
     The SSRF guard, the key's limits, the daily budget, robots.txt as
     the site's policy says and the cache apply as at every level.
   - **1, http-emulated**: a persona's http client (wreq with the
     persona's verified browser profile, header tables and cookie jar),
     the persona's pacing. Refused with `level_unavailable` while no
     verified major exists (F30), so that nothing is silently downgraded.
   - **2, browser-fast**: the persona's real browser, a plain load: no
     scrolling, no wandering pointer, no patience, the dwell at the site
     policy's `min_dwell` only; the persona's pacing, visits and nights
     stay, because they are the address's rhythm, not the page's.
   - **3, human**: everything of 1.0 (D37, D40, D44, D45).
2. **A site's policy sets a floor**: `min_level` (default 0) in
   `policies/domains.yaml`; a request below it is refused
   (`level_below_policy`), never raised silently. The shipped policy
   file names a floor of 3 for the detection pages and the runbook says
   when to raise a site's floor.
3. **Egress is the operator's choice, global**: `egress.mode` is
   `proxies` (1.0's behaviour, the default) or `direct`. Under `direct`
   the host's own connection carries the traffic and **one persona**
   stands for it (`egress.direct.personas`, default 1, at most 4): its country,
   time zone, locale and languages come from the host's observed geo
   through the same lookup the pool uses, or from
   `egress.direct.country` and `locale` when declared; it is generated,
   warmed, paced and self-tested like any persona, and the forwarder
   still fronts every browser and client (the SSRF guard, the byte
   count) with no upstream. There is no mixed mode: a site that sees one
   visitor move from a residential address to the host's has seen a bot,
   so the pool and the host's address are never used for one site by
   turns.
4. **What a level changes and what it never changes.** The persona
   handle, sessions, jobs, crawls, flows, webhooks, extraction, robots,
   the cache and the results keep their shape; a result says the level
   applied; `persona` is null at level 0; a session cannot be opened at
   level 0 (`session_needs_persona`); a job's `defaults.level` applies to
   its items and a crawl's to its pages; a flow is level 2 or 3 (a flow
   is a browser). Metrics carry a `level` label.
5. **2.0.0.** The required field breaks every 1.x request on purpose:
   there are no outside consumers of 1.0, and a default would quietly
   decide the one thing the owner wants decided explicitly.

## Consequences

- `CLAUDE.md`'s rule becomes: no request leaves the host's own address
  unless the operator configured `egress.mode: direct` or
  (`egress.plain` is `host | pool` and is `host` only under `direct`);
  under `proxies` nothing does, as before.
- D40's floors apply at level 3 and, for the dwell, at level 2; level 0
  has no persona to protect, and level 1 has no page behaviour.
- The runbook and the OpenAPI say what each level costs and what a site
  can tell at each; the README says it in plain words: below 3, Kage is
  not pretending to be a person, it is a well-behaved client.
- The security review gains the direct mode's threat (the operator's own
  address is what a site sees and may block) and level 0's (a caller
  with the `fetch` scope can make the host's address request any page
  the guard allows, at the key's rate).
