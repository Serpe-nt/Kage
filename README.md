# Kage

*A shinobi is not invisible. A shinobi is unremarkable.*

[![CI](https://github.com/Serpe-nt/Kage/actions/workflows/ci.yaml/badge.svg?branch=main)](https://github.com/Serpe-nt/Kage/actions/workflows/ci.yaml?query=branch%3Amain)
[![Release](https://img.shields.io/github/v/release/Serpe-nt/Kage?sort=semver&display_name=tag)](https://github.com/Serpe-nt/Kage/releases/latest)
[![Licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue)](#license)
[![Rust](https://img.shields.io/badge/rust-1.99.0-orange)](rust-toolchain.toml)
[![Coverage](https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2FSerpe-nt%2FKage%2Fgh-pages%2Fcoverage.json)](docs/runbooks/repo.md#badges)

**Kage** (影, Japanese for *shadow*: it goes where its person goes, at its
person's pace) is a self-hosted API that fetches web pages. On every
request the caller says how much of a person it should be, by a
camouflage **level**: 0 is a plain HTTP client that says it is Kage, 1 an
emulated browser client (waiting for a verified profile), 2 a real browser
that loads the page and leaves, 3 a long-lived persona at a real browser on
a household connection that scrolls, reads, pauses and clicks as the
person it stands for would. Below 3 Kage is a well-behaved client, not a
person; a site's policy may refuse a level below the one its operator set.
Nothing is ever raised or retried at another level.

Behind level 3 is everything a site can measure: static residential
proxies from any provider (or the host's own connection, never both), one
persona per address with a fingerprint drawn from real devices and kept for
its life, Chrome 155 and Camoufox 156 in a hardened sidecar with the
persona's own hand on a virtual display, pacing with breaks and nights, and
a detection score that Kage measures against public test pages instead of
promising. It runs as one `docker compose` stack with a REST API (single
fetches, batches, scripted flows, crawls, extraction, sessions, webhooks),
metrics and traces; it holds no credentials for any site, never logs in,
never solves a CAPTCHA, and never pretends to be a person below level 3.

## Who it is for

- **Security operations**: seeing your own pages and defences as an
  ordinary visitor from an ordinary address sees them.
- **Platform and dev-ops teams**: reading what your own sites serve, at a
  pace and in a shape they would accept from a person.
- **Penetration-testing and research teams with written authorization for
  their targets**: assessments of bot defences, scraping exposure and what
  a site gives away to a browser.
- **An operator's own projects**: fetching public pages at a human pace.

Authorization is the caller's. Kage records none, checks none and holds
none for any site.

## Responsible use

Kage is built for fetching publicly available pages at a human pace.
Whoever runs it is responsible for how it is used.

- **Authorization is yours.** Read only what you may: your own sites,
  public pages, and targets you hold written authorization for. Kage
  cannot tell them apart and does not try.
- **Follow the law and the site's terms.** Scraping rules differ by country
  and by site. Check both before pointing Kage at a target, and stop when a
  site asks you to.
- **A level is how you appear, not whether you may.** Below level 3 Kage
  says what it is: level 0 sends `User-Agent: kage/<version>` unless you
  name another client, and levels 1 and 2 behave like no person. A site's
  floor (`min_level` in its policy) is the operator's to set.
- **Stay polite.** Pacing defaults imitate one human visitor per persona.
  Do not raise them to extract more than a person could read.
- **`robots.txt` is the operator's choice, per site.** A reader never asks
  for that file, so asking marks the address that does. Kage therefore
  asks for a site's `robots.txt` only where the domain policy says
  `report` or `enforce`, and then from one address that never reads the
  site. Under `enforce` it obeys the file fully: what is disallowed is
  not fetched, and a `Crawl-delay` is kept to, however long. Where the
  policy says nothing, every response says that the file was not asked
  for. Decide per site, and decide `enforce` where a site's rules are
  what you mean to follow.
- **No access you were not given.** Kage does not solve CAPTCHAs through
  external services, does not log in with accounts, and does not perform
  write actions on targets. Under `egress.mode: direct` every site sees
  your own address, and then no proxy is used at all: never both for one
  site. These are design decisions, not missing features; please do not
  send patches that change them.
- **Handle personal data with care.** Fetched pages can contain personal
  data. Retention defaults are short. Store only what your project needs and
  delete it when you are done.
- **Do no harm.** Do not use Kage to overload a site, to evade a ban that was
  placed on you for abuse, to commit fraud, or to collect data about people
  for harassment or surveillance.

The settled decisions behind these limits are in
[docs/00-decisions.md](docs/00-decisions.md) (D10, D13, D46, D50),
[docs/adr/0006-no-solvers-no-direct-mode.md](docs/adr/0006-no-solvers-no-direct-mode.md)
(whose never-direct rule [ADR 0012](docs/adr/0012-camouflage-levels-and-egress.md)
replaced with the operator's choice of one egress) and
[docs/adr/0012-camouflage-levels-and-egress.md](docs/adr/0012-camouflage-levels-and-egress.md).

## Quick start

On a Linux machine (or WSL2) with Docker Engine 27 or newer, Compose v2,
git, curl and bash, and nothing else:

```bash
git clone https://github.com/Serpe-nt/Kage.git kage && cd kage
git checkout v2.0.0
scripts/dev-secrets.sh
```

`scripts/dev-secrets.sh` writes random local secrets into `deploy/.env`
(ignored by git; keep it). Then say where requests leave from, with one
line in that file:

```bash
echo 'KAGE_EGRESS_MODE=direct' >> deploy/.env
```

That is **this machine's own connection**, the way that needs nothing but
Docker: one persona stands for the machine, placed where a lookup of its
address finds it, and **every site you fetch sees your address**. The other
two ways are the proxy pool, one persona per proxy:
`KAGE_WEBSHARE_API_KEY=<key>` for a Webshare account, or
`KAGE_STATIC_LIST_FILE=<path>` for a declared list of any vendor's proxies
([Three ways to run](#three-ways-to-run-the-pool)). A new persona warms up
for a few hours, reading popular neutral sites as a new household browser
would, before it reads yours; for a try-out, skip that with
`echo 'KAGE_PERSONAS_WARMUP=false' >> deploy/.env`.

```bash
docker compose --file deploy/compose.yaml up --build --detach --wait
until curl --silent --fail http://127.0.0.1:8080/readyz > /dev/null; do sleep 5; done
```

The first build compiles Kage and fetches the pinned Chrome and Camoufox:
about ten minutes on 32 cores, up to 40 on a small machine. The stack
applies the database migrations before `kage` starts and publishes one
port, on loopback only: the API speaks plain HTTP, and a deployment that
serves other machines puts a reverse proxy that terminates TLS in front of
it. `/readyz` answers `200` once a persona is ready.

Make a key (its token is shown once) and fetch a page of a site you may
fetch, at level 0 and at level 3:

```bash
docker compose --file deploy/compose.yaml exec kage \
    kage key create --name operator --scope admin --scope fetch
```

```bash
curl http://127.0.0.1:8080/v1/fetch \
    --header "Authorization: Bearer <token>" \
    --header 'Content-Type: application/json' \
    --data '{"url": "https://example.org/", "level": 0}'
```

```bash
curl http://127.0.0.1:8080/v1/fetch \
    --header "Authorization: Bearer <token>" \
    --header 'Content-Type: application/json' \
    --data '{"url": "https://example.org/", "level": 3}'
```

The first answers at once; the second takes as long as a person reading
the page, tens of seconds. Under `direct` both came from your own address:
on a real site choose one level, because an address that says it is Kage
and then reads like a person is a bot that said so, and set the site's
floor so that nobody does both
([levels-and-egress.md](docs/runbooks/levels-and-egress.md)).

[docs/runbooks/first-start.md](docs/runbooks/first-start.md) is the whole
first start, [docs/runbooks/api.md](docs/runbooks/api.md) the guide to
keys, limits, jobs, crawls, sessions and the admin endpoints. The released
images, signed and attested, run by digest without building kage or the
Chromium sidecar ([deploy/compose.yaml](deploy/compose.yaml) says how).

## The levels

| Level | Name | What runs | Persona | What a site sees |
|---|---|---|---|---|
| 0 | plain | an HTTP client with its own TLS, `User-Agent: kage/<version>` unless the request names one, the caller's headers and body as given, no cookie jar | none | a well-behaved HTTP client that says who it is |
| 1 | http-emulated | the persona's HTTP client with its browser's TLS and HTTP/2 fingerprint, headers and cookies | yes | a browser's wire fingerprint with no page behaviour; **not available yet**: refused with `level_unavailable` until a browser version Kage runs has a verified profile |
| 2 | browser-fast | the persona's real browser loads the page, sends no input, stays for the site's `min_dwell` and leaves | yes | a real browser that loads a page and leaves |
| 3 | human | the persona's browser as a person: it scrolls as far as its patience lasts, reads, rests, moves the mouse and clicks links as a reader does, at the persona's pace, with breaks and nights | yes | a person |

In plain words:

- **Below 3 Kage is a well-behaved client, not a person.** Level 0 is for a
  page you could read with `curl`; level 2 for a site that needs a browser
  but does not watch how it is read; level 3 for a site that watches.
- **Every request names its level**, and one without it is refused
  (`level_required`). Nothing is decided for the caller.
- **Nothing escalates.** The level is the request's ceiling: a block at
  level 0 is the answer, never a reason to try a browser.
- **A site's floor is the operator's guard.** A request below the
  `min_level` of the site's policy is refused (`level_below_policy`),
  never raised:

  ```yaml
  # policies/domains.yaml
  domains:
    "shop.example.org":
      min_level: 3
  ```

- **What it costs:** a level-0 page is one request; a level-2 page a load
  and a few seconds; a level-3 page about 35 seconds at the median, about
  a hundred pages an hour per persona, plus the pacing between pages.

[docs/runbooks/levels-and-egress.md](docs/runbooks/levels-and-egress.md)
has the whole table: what each level takes of a request, what it costs,
what a site can tell, and how to upgrade from 1.x.

## Where requests leave from

| | `egress.mode: proxies` (the default) | `egress.mode: direct` |
|---|---|---|
| Personas | one per proxy of the pool | one to four standing for the host |
| Level 0 | from one address per site: a plain exit of the pool, else the site's scout's | from the host |
| The host's own address is seen by | no site: only the provider's API, the webhooks and the host's own lookups | every site |

**There is no mixed mode**, and `kage config check` refuses one: a site
that sees a visitor move from a residential address to the host's has
seen a bot. The forwarder inside `kage` fronts every browser and client
in both modes, so the SSRF guard and the byte count always hold.

## Three ways to run the pool

| | Webshare | A declared list | No proxies |
|---|---|---|---|
| Set in `deploy/.env` | `KAGE_WEBSHARE_API_KEY=<key>` | `KAGE_STATIC_LIST_FILE=<path>` | `KAGE_EGRESS_MODE=direct` |
| A persona's place | where a geo lookup finds the address | what each entry declares (`country`, optional `locale`, `timezone`), verified by the lookup | where a lookup finds the host, or `egress.direct.country` |
| Replacing a burned address | yes, automatically within a monthly budget | no: the list is yours to edit | no |
| What else | the replaced log, the account, the traffic stats | any vendor; an entry without a country carries no persona and serves level 0 only | every site sees your address |

A declared list is one `host:port:username:password:country[:locale]` per
line, or YAML with more fields, read again at every sync; it holds
passwords, so let only the container's user read it (`uid 65532`). Its
probes read a public echo (`checkip.amazonaws.com`), whose operator sees
every proxy address, unless you name your own. Kage takes Webshare when its
key is set, else the list (`proxies.provider: auto`), and `kage config
check` says which and what it cannot do. A vendor whose API brings more
than a list can is a provider module of its own:
[CONTRIBUTING.md](CONTRIBUTING.md#adding-a-proxy-provider).
[docs/runbooks/proxies.md](docs/runbooks/proxies.md) is the operator's guide
to the pool, [docs/runbooks/personas.md](docs/runbooks/personas.md) to the
personas.

## What a result holds

A fetch answers `200` with its result whenever something was fetched; a
block is an outcome, not an HTTP error. Trimmed:

```json
{
  "id": "0192…",
  "state": "finished",
  "outcome": "ok",
  "level": 3,
  "status": 200,
  "final_url": "https://example.org/article",
  "headers": [{ "name": "content-type", "value": "text/html; charset=UTF-8" }],
  "dom_html": "<!doctype html>…",
  "persona": { "id": "p_3f9a6c0d12e4b87a", "engine": "chromium", "claimed_os": "linux", "country": "DE" },
  "attempts": [
    { "n": 1, "render": "browser", "outcome": "ok", "status": 200, "total_ms": 41250,
      "persona": "p_3f9a6c0d12e4b87a", "engine": "chromium", "exit": null, "reason": null }
  ],
  "behavior": { "scroll_reached": 0.84, "scroll_ended_by": "patience", "patience_ms": 38900,
                "dwell_ms": 6100, "moves": 23, "notches": 41, "referer_strategy": "direct" },
  "block": null,
  "robots": { "fetched": false, "allowed": true, "matched_rule": null, "crawl_delay": null },
  "cached": false
}
```

- **`outcome`** says how it ended: `ok`, `challenge_passed`, or a block
  (`challenge_failed`, `captcha`, `forbidden`, `rate_limited`, …) with
  **`block`** saying who blocked and how; `provider_error` with a `reason`
  when nothing could be attempted.
- **`level`** is the level the request was made at.
- **`persona`** is a handle that is stable for your key and means nothing
  to another; a key without the `admin` scope never sees a proxy, an
  address, a seed or a fingerprint. At level 0 it is `null`, and each
  attempt says its `exit` (`host` or `pool`) instead.
- **`attempts`** lists every try, by whom, how it ended and how long it
  took; **`behavior`** what the persona did on the page.
- **`include`** chooses what comes back and is kept: `dom`, `raw`,
  `headers`, `extract` (the main content as text and markdown, metadata
  and links), `xhr`, `screenshot`, `pdf`, `console`.
- **`robots`** is the site's `robots.txt` verdict; **`cached`** whether the
  page came from the response cache.

Batches, scripted flows and crawls give the same result for each page
([api.md](docs/runbooks/api.md)). The contract is the OpenAPI document,
served at `/openapi.json` and committed as
[crates/kage-api/openapi.json](crates/kage-api/openapi.json).

## Measured, not promised

The engines of 1.0.2 were checked live through residential proxies: a
Chrome 155 persona claiming Linux (2026-10-19) and a Camoufox 156 persona
claiming Windows (2026-10-18) each scored **100 of 100** on the 34 checks
Kage reads of [bot.sannysoft.com](https://bot.sannysoft.com/) and
[CreepJS](https://abrahamjuliot.github.io/creepjs/). CreepJS's own figures
were **0 % headless and 0 % stealth** for both, and **"like headless" 38 %**
for Chrome and 6 % for Firefox. The score and its threshold are Kage's own
(`min_score` 90); the 38 % is shown because hiding it would be the
opposite of measuring. The recorded runs are the baselines in
[tests/fixtures/selftest/](tests/fixtures/selftest/), and a run that is
worse on any check fails.

A detection page is not a site that watches: a pass there is necessary,
not sufficient, and nothing here says Kage cannot be told apart. The live
suite needs real proxies and runs only on the maintainer's word; no
nightly live measurement is published.

What the gates measure: on every pull request, the line coverage of the
badge above, both engines through one parity suite of the same contract,
capture and behaviour tests, and the wire of the HTTP clients against
recorded fingerprints; before a milestone is merged, the mutation score of
`kage-core` (93.4 % at the last full run, against a gate of 70 %).

The honest limits, each written down where it is decided:

- **Hardware-rooted attestation** (Private Access Tokens and the like)
  cannot be passed by any browser, and some vendors probe the automation
  transport itself ([docs/00-decisions.md](docs/00-decisions.md), "Honest
  limits").
- **The canvas is the host's** (T1): every persona of one host draws the
  same canvas pixels and text metrics while each names its own graphics
  card ([Known tells](docs/security-review.md#known-tells)).
- **WebGL comes from Mesa's software renderer** (T7), in a configuration no
  stock Chrome reaches by itself, the same for every persona of a host; the
  answer is the host's own GPU ([Known tells](docs/security-review.md#known-tells)).
- **Google's properties see a Chrome that never fetched a variations seed**
  and sends no `X-Client-Data` header (T5), because background networking
  is off ([Known tells](docs/security-review.md#known-tells)).
- **Chrome's built-in AI APIs answer `unavailable`** (T6), so that a page's
  click cannot make a persona download a model through the pool
  ([Known tells](docs/security-review.md#known-tells)).
- **Chrome personas claim Linux**, the system they run on
  ([ADR 0009](docs/adr/0009-chromium-personas-claim-linux.md)).
- **Level 1 waits** for a verified HTTP profile of a browser version Kage
  runs; a default deployment offers levels 0, 2 and 3.
- **Under `direct` one address does everything**, and below level 3 Kage
  is not a person, by design.

## Configuration and the command line

`kage` reads `kage.yaml` (or the file named by `--config` / `KAGE_CONFIG`)
and lets environment variables of the form `KAGE__SECTION__KEY` override it.
[deploy/kage.example.yaml](deploy/kage.example.yaml) lists every key with its
default. Secrets are never read from the file: they come from the environment
or from a file named by the same variable with `_FILE` appended, and `kage`
refuses to start when a required one is missing. Every YAML file Kage reads
refuses a repeated key and a merge key, and refuses unquoted text that looks
like a number where text is expected (`password: "0123"`).

```bash
kage config check   # validate, then print the effective configuration, secrets redacted
kage proxy list     # the pool; also `health`, `replace` and `egress`
kage persona list   # the personas; also `show`, `selftest`, `regenerate` and `retire`
kage fetch <url> --level <0-3>   # one request through the whole pipeline
kage key create     # an API key; also `list`, `revoke` and `rotate`
kage serve          # the API, the pool, the personas and the scheduler
```

Per-domain behaviour is data in [policies/](policies/): `domains.yaml` (a
site's floor, pacing, robots, retries, cooldowns), `pacing-profiles.yaml`
and `block-signatures.yaml`, reloaded when they change; an invalid file
keeps the last good policy in force and raises an alert.

## Tools of the trade

| Document | What it holds |
|---|---|
| [docs/01-spec.md](docs/01-spec.md) | the contract: every behaviour, by section |
| [docs/00-decisions.md](docs/00-decisions.md) | the settled decisions, D1 to D53 |
| [docs/adr/](docs/adr/) | why things are the way they are |
| [docs/02-plan.md](docs/02-plan.md) | the milestones and what each one built |
| [docs/versions.md](docs/versions.md) | every pinned version, the fingerprint-critical ones apart |
| [docs/runbooks/](docs/runbooks/) | the operator's guides: first start, proxies, personas, fetching, levels and egress, the API, rotating keys, upgrading an engine, reading a block report, the fingerprint echo, observability, the repository and its releases |
| [docs/security-review.md](docs/security-review.md) | threats, findings, accepted exceptions and the known tells |
| [crates/kage-api/openapi.json](crates/kage-api/openapi.json) | the public API's OpenAPI 3.1 document |
| [CHANGELOG.md](CHANGELOG.md) | what changed, release by release |

## Joining the clan

Contributions are welcome: a fix, a provider for another proxy vendor, a
sharper test, a document that says something better. Start with
[CONTRIBUTING.md](CONTRIBUTING.md): the gates (`just setup`, then
`just ci` runs what a pull request runs), the pull-request rules, the
definition of done, and a step-by-step guide to adding a proxy provider.
Building Kage itself needs rustup, Node.js 24 with corepack and
[just](https://just.systems) 1.58 or newer; `just setup` installs the
rest. Everyone taking part follows the
[code of conduct](CODE_OF_CONDUCT.md). The repository's settings, the CI
jobs and the release procedure are written down, with the commands that
apply them, in [docs/runbooks/repo.md](docs/runbooks/repo.md).

## Security

Please report a vulnerability privately through GitHub's private advisory
form ("Report a vulnerability" on the repository's Security tab), never in
a public issue. [SECURITY.md](SECURITY.md) says what is in scope, which
versions get fixes, and which known tells are documented limits rather
than vulnerabilities.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution you
intentionally submit for inclusion in this work, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

Third-party components keep their own licenses. Two things about the Firefox
sidecar need saying, both recorded in
[docs/adr/0008-agpl-ua-parser-js-in-firefox-engine.md](docs/adr/0008-agpl-ua-parser-js-in-firefox-engine.md):

- `camoufox-js` asks for `ua-parser-js` 2.x, which is AGPL-3.0-or-later. The
  workspace uses the MIT-licensed 1.0.x line instead; the dependency tree has
  no AGPL package.
- **The Firefox sidecar image is not published.** It is built from Camoufox,
  whose build bundles Microsoft's and Apple's font files (they are what makes
  a Windows or macOS persona's fonts right), and a public image of this
  project would redistribute them. Build it where it runs:
  `just build-images`. This is the project's cautious default, not legal
  advice.
