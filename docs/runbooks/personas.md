# Personas

A persona is one long-lived synthetic household user: one browser, one
screen, one language, one place, behind one proxy (spec §5, §6). The
persona manager (`kage-persona`, started by `kage serve`) makes one for
every usable proxy and keeps it in step with that proxy for the rest of
its life. Under `egress.mode: direct` its "proxy" is one of the host's
own rows (below, "Host personas"). This runbook says what it does by
itself, what an operator can do, and what each event and alert means.

A persona serves levels 1 to 3; a request at level 0 has none
([levels-and-egress.md](levels-and-egress.md)).

## What a persona is made of

| Attribute | Where it comes from |
|---|---|
| seed | Derived from the persona's id; stored; never changes. Everything below that is drawn hangs on it. |
| engine and system | `personas.mix` (default: chromium on linux 40, firefox on windows 45, firefox on macos 15), drawn by what the pool is short of, the persona's own country first. Twenty proxies give 8, 9 and 3 give or take one (two at the most), and no country is all one browser. |
| country, time zone, position | For a provider that states where its addresses are (Webshare): the observed geo record of the proxy (`kage proxy health`), the provider's country only while no lookup has succeeded. For a **declared** entry (a declared list, a host row): the declaration, see below. The position is a seeded point 0.5 to 4 km from the lookup's coordinates. |
| locale and languages | A table per country (`kage_core::persona::locale`): the locale carries the country (`de-AT`), the language list is the one a stock installation of that browser ships with there. Chrome in Austria sends `de-DE, de, en-US, en`; Firefox sends `de, en-US, en`. A declared locale (`fr-CH`) picks the browser's list for its language (Chrome `fr-FR, fr, en-US, en`). A country without an entry gets `en-US, en` and the event `persona_locale_unknown`; a declared country or locale without one is refused when the list is read. |
| screen | A class from a seeded table per engine and system, asked of the generator by exact bounds. |
| fingerprint | Generated once by the engine's sidecar from the seed and the constraints above, stored whole, sent back unchanged on every open. |
| http profile | The wreq-util profile of the same browser major, if one exists (`kage config check` lists them). Chrome 155 and Firefox 156 have none: their personas are browser-only (`http_profile_missing`, once per engine and major), and level 1 is refused (`level_unavailable`). |
| profile directory | `<engine>/<persona id>` on the personas volume. |

Nothing of this is ever drawn again. `tables_version` records which tables
a persona came from; new tables only apply to new personas.

### Declared entries

An entry of a declared list (and a host row with `egress.direct.country`)
**declares** where its address is; the geo lookup only verifies it
(ADR 0013, [proxies.md](proxies.md#a-declared-list)):

- **Country and locale** are the declaration's, whatever a lookup says.
  Without a declared locale the country's table gives it.
- **Time zone**: the declared `timezone` when there is one
  (`timezone_source: declared`); else the lookup's zone when the lookup
  agrees on the country; else the country's main zone.
- **Position**: from the lookup's coordinates, only when the lookup agrees
  on the country; else none.
- **A lookup that disagrees twice** (`proxy_geo_mismatch`) leaves the
  persona as it is: it keeps the declared identity, the proxy is
  `degraded` and still serves. Correct the declaration, or leave the
  country out to make the entry a plain exit, or switch the lookup off.
- **A corrected declaration** is followed at the next sync: a new country,
  or a locale in another language, retires the persona
  (`declaration_changed`) and the entry gets a new one; a new time zone or
  city only moves the persona's zone.
- **No country, no persona.** An entry without a country is a plain exit
  for level 0 ([levels-and-egress.md](levels-and-egress.md#under-the-pool-egressmode-proxies));
  a persona whose entry loses its country is retired (`undeclared`).
- **One address, one persona.** A proxy that leaves by an address an older
  proxy already leaves by carries nothing (`proxy_exit_shared`), and the
  persona it had is retired (`exit_shared`).

## Commands

All of them may run next to the server.

```bash
kage persona list                 # live personas and their spread over the mix
kage persona list --all --json
kage persona show <id>            # identity, self-test runs, per-domain state
kage persona show <id> --json --fingerprint
kage persona selftest <id>        # its own context, the operator's own echo and pages
kage persona selftest <id> --third-party
kage persona regenerate <id>      # a new persona behind the same proxy
kage persona retire <id>
kage selftest http --persona <id>
kage selftest browser --persona <id> [--third-party]
```

`retire`, `regenerate` and `selftest` leave an audit event.

- **retire.** The persona is gone for good: its sessions end and its http
  cookies are deleted at once. Its proxy gets a new persona at the server's
  next sweep (one minute), which inherits nothing.
- **regenerate.** Retire plus a new persona at once. Use it for a persona
  whose fingerprint you do not trust any more; never to "freshen" one
  that works, because a household that changes its browser, its screen and
  its cookies overnight behind the same address is exactly what a persona
  is meant not to be.
- **selftest.** Runs as the persona through its own proxy. By default it
  asks `selftest.http.echo_url` and loads `selftest.browser.pages`, the
  operator's own. `--third-party` uses `selftest.http.third_party_url` and
  adds `selftest.browser.third_party_pages`: those sites see the exit
  address of the persona's proxy, so only this command and the gated live
  suite ever use them. The command exits non-zero when a path fails; a
  path the persona does not have is reported as not run.

## What the server does by itself

- **Generation and follow-up.** On the pool's events, and at every sweep
  (`personas.sweep_interval`, one minute) for whatever no event announced.
- **Warm-up** (`personas.warmup`, on by default; off under `just dev` and
  in every test). A new persona makes 6 to 10 visits to popular neutral
  sites in its own browser, with the behavior engine on: the first 3 to 20
  minutes after it was generated, then with gaps of about 25 minutes, only
  between 07:00 and 23:30 of its own time zone. It is given real work only
  once `warmed_at` is set, after three visits or twelve hours; until then
  `kage persona list` shows `active (warming up)`. A blocked visit is
  recorded, cools that site for that persona and is never repeated. The
  default sites are the language edition of Wikipedia, OpenStreetMap and
  the national public broadcaster and weather service of AT, DE, FR, GB
  and IT; change `personas.warmup.sites` for other countries.
- **Scheduled self-tests.** Each path of each persona every 24 hours, give
  or take a quarter, the first a few minutes after generation. Only the
  operator's own echo and pages are ever used on a timer. **With the
  shipped configuration neither is set, so nothing is scheduled**
  (`kage config check` shows `personas.scheduled_selftests`). Host the
  coherence probe and the echo (`docs/runbooks/fingerprint-echo.md`,
  `docs/runbooks/engine-chromium.md`) and name them in `selftest.http.echo_url`
  and `selftest.browser.pages` to have every persona checked daily.
- **Archive.** Ten minutes after a persona was retired
  (`personas.archive_after`, longer than the sidecar's idle close) its
  profile directory is moved to `.archive/<persona id>` on the personas
  volume, with a `persona.json` that describes it (no credentials), and
  the archive is deleted after `personas.retention_days` (7). The row, the
  fingerprint, the per-domain state and the self-test runs stay in the
  database. Under `just dev` kage does not see the volume: a warning says
  so once, nothing is archived, and the profiles stay in the volume of
  the dev stack.
- **A newer browser.** When an engine image bundles a newer browser
  major, each persona of that engine moves to it at its next open, as a
  real browser's update does: the sidecar moves its document (the browser
  version and the `User-Agent`; the device, the place and the seed stay),
  kage stores it with the new `browser_major` and records
  `persona_browser_upgraded`. Sessions, cookies, the http jar and what
  the persona did on each site continue. A Firefox profile is copied to
  `.upgrade/<persona id>` on the personas volume before its first start
  under the new major, and kage deletes the copy after
  `personas.upgrade_backup_days` (7): that copy is the only way back to
  the previous image (`docs/runbooks/upgrading-an-engine.md`).
- **Probes.** A health probe through a proxy presents as the persona
  behind it when that persona has an http path, on a fresh connection and
  without its cookies. A host persona's row is never probed.

## Host personas (`egress.mode: direct`)

Under `direct` the host's own connection carries the traffic, and
`egress.direct.personas` personas (1 by default, at most 4) stand for it,
each on a row of its own (provider `host`, [proxies.md](proxies.md#the-hosts-own-rows-egressmode-direct)).
They are generated, warmed up, paced and self-tested like any persona,
with three differences:

- **Their place** is the host's: its own geo lookup, read directly at
  start and once per `proxies.health.geo_interval`, or what
  `egress.direct.country` and `egress.direct.locale` declare, which win
  (a lookup then only verifies them). Without either there is no host
  persona and only level 0 works.
- **Their engines** are drawn from `egress.direct.engines` when it is
  set, else from `personas.mix`.
- **They share one address**, the host's. A site sees every host persona
  and every level-0 request from it; a lone host persona also asks a site
  for its `robots.txt` itself. What that costs and how `min_level` guards
  against it is in [levels-and-egress.md](levels-and-egress.md#the-hosts-own-connection-egressmode-direct).

The pool's personas are not scheduled under `direct` and wait; the host
personas are retired, with their rows, at the first start under
`proxies` (`egress_mode`). Never both at once.

## States

| State | Meaning | Ends when |
|---|---|---|
| `active` | Serving, once warmed up. | |
| `cooling (proxy_backoff)` | Its proxy is in backoff or being replaced. No new sessions; open ones go on. | The proxy is usable again. |
| `cooling (selftest_failed)` | A self-test showed an automation flag or an identity that contradicts the persona. | A self-test passes: the next scheduled one (two hours later) or `kage persona selftest`. |
| `cooling (geo_changed)` | The address moved to another country and the persona's locale and languages followed it. | A self-test passes. |
| `retired` | Its proxy is gone, or an operator said so. Final. | |

## Events and alerts

Health events about a persona have `subject_kind = persona`.

| Event | Severity | Meaning and action |
|---|---|---|
| `persona_generated` | info | Engine, system, country, time zone and where it came from, locale, screen, profile. |
| `persona_locale_unknown` | warning | The country has no entry in the locale table; the persona speaks `en-US, en`. Add the country to the table (a code change) if the pool will keep addresses there. |
| `persona_state_changed` | info | From, to, the reason and the proxy's state. |
| `persona_geo_updated` | info or warning | The geo record of the address changed. Info: same country, time zone and position followed. Warning: another country; locale and languages followed and the persona cools until a self-test passes. If the address really moved abroad for good, consider `kage proxy replace`: a household does not emigrate. |
| `persona_warmup_visit`, `persona_warmed` | info (warning for a visit that did not arrive or was blocked) | One visit and its outcome; the persona is ready. |
| `persona_selftest_passed`, `persona_selftest_failed`, `persona_selftest_inconclusive` | info, warning, warning | One run. Inconclusive: the test site could not be read; nothing changes. |
| `persona_retired` | info | Why (`proxy_retired`, `regenerated`, `operator`; for declared entries `declaration_changed`, `undeclared`, `exit_shared`), and how many sessions and cookies went with it. |
| `persona_profile_archived`, `persona_profile_deleted` | info | Where the archive is; `archive: null` means there was no directory within reach. |
| `persona_browser_upgraded` | info | The persona moved to the browser major its engine runs: engine, `from_major`, `to_major`, the browser version and the wreq profile of the new major (null: browser-only). One per persona and major; expected for every persona of an engine after an image with a newer browser is deployed. |

| Alert | Severity | Action |
|---|---|---|
| `persona_generation_failed` | critical | A usable proxy has no persona, so nothing is fetched through it. The message carries the sidecar's reason. A full or unreachable sidecar raises it on the third failure in a row; the manager tries again every sweep. |
| `browser_selftest_failed` | critical | Read the run (`kage persona show <id>`, `selftest_runs.findings`). A hard finding is an automation flag or a value that contradicts the persona. If it reproduces with `kage persona selftest <id>`, compare with a second persona of the same engine: one persona failing is its fingerprint (`regenerate`), all of them failing is the engine (stop, see the engine's runbook). |
| `http_selftest_failed` | critical | The http path's JA4, HTTP/2 fingerprint or header order differs from the recorded one: a pin moved or a proxy rewrites traffic (`docs/runbooks/fingerprint-echo.md`). |
| `http_profile_missing` | warning | Expected for Chrome 155 and Firefox 156; see `docs/versions.md`. |

## Sessions and per-domain state

A session pins a caller's requests to one persona, so a request at level 0
cannot name one (`session_needs_persona`). Ids are the caller's or
minted by the server and unique per API key. A session ends after
`sessions.idle_ttl` (10 minutes) without use, `sessions.max_lifetime`
(6 hours) after it was opened, or at once when its persona is retired; it
never moves to another persona. A new session goes to the ready persona
with the fewest open sessions.

Per domain, a persona has a cooldown after a block, a daily request count
that turns over at midnight of its own time zone, and a count of blocks in
24 hours (`kage persona show <id>`). The scheduler that reads them arrives
with M7.

## The live check

`tests/live/personas.sh` is part of the gated live suite but not of the
nightly run: `KAGE_LIVE=1 KAGE_LIVE_PERSONAS=1 just test-live` adds it. It
starts `kage serve` on a throwaway database with warm-up off, waits until
every usable proxy of the real pool has its persona, checks them
(`tests/live/check-personas.mjs`: one persona per proxy, each where its
proxy was observed to be, the mix within two personas of its shares, no
shared seed, at least three screens), then runs
`kage persona selftest --third-party` for one Chrome and one Firefox
persona through their own proxies and judges the reports with the recorded
baselines of the engines' live checks. It needs the Webshare key and the
master key, and it shows two exit addresses to the third-party detection
pages. It never touches a target site. The Firefox persona it tests is one
that claims Windows when the pool has one, the system the baseline was
recorded with. With `KAGE_LIVE_EVIDENCE_DIR` it keeps the persona table and
what the judge printed, which holds no address; the raw reports are not
kept.

It last ran on 2026-10-10, against the real pool of twenty proxies (four
each in AT, DE, FR, GB and IT) and passed:

- Twenty personas existed two and a half minutes after `kage serve`
  started, one per proxy, as fast as the health checks placed the
  addresses: 8 chromium on Linux, 8 firefox on Windows, 4 firefox on macOS
  (the mix asks for 8, 9 and 3), both browsers in every country, six
  different screens. Every persona had the time zone and the locale of the
  country its address was observed in and the language list its browser
  ships with there (a Firefox in Germany or Austria `de, en-US, en`, a
  Chrome in Austria `de-DE, de, en-US, en`).
- A Chrome persona in Austria and a Firefox persona claiming Windows in
  Germany each passed `kage persona selftest --third-party` through its
  own proxy with a score of 100: every row of bot.sannysoft.com passes,
  CreepJS reports headless 0 % and stealth 0 % for both and like headless
  38 % for Chrome and 6 % for Firefox, and no check is worse than in the
  recorded baselines (`tests/fixtures/selftest/`).
- No persona visited a site by itself (warm-up was off, and no self-test
  is scheduled without pages of the operator's own), and no replacement
  was requested.

## Limits to know

- A chromium persona's processor count is clamped to the engine host's
  CPUs when its context opens (ADR 0009); on a small host every chromium
  persona reports the same count (plan F25).
- The manager's own browser work and an operator's `kage persona selftest`
  open the same context of a persona. Avoid running the command for a
  persona in the minutes its warm-up visit or scheduled self-test is due;
  a run that collides is refused or inconclusive, never wrong (plan F27).
- One device record in ten of the Firefox generator has a work area as
  large as its screen (no taskbar). The probe reports it as a warning
  (`environment.*`); it is a real device's quirk and is left as drawn.
