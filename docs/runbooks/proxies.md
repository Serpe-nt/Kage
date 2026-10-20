# Runbook: the proxy pool

How the pool gets its proxies, what it does with them, and what an operator
does when it raises an alert. Spec §7 is the contract and ADR 0013 the
decision behind the providers and the declared list; this page is the
practice.

## Three ways to run

| | Webshare | A declared list | No proxies |
|---|---|---|---|
| What carries the traffic | Webshare's static residential proxies | the proxies of any vendor, listed by the operator | the host's own connection (`egress.mode: direct`) |
| Where the list comes from | the Webshare API, synced every 10 min | `KAGE__PROXIES__STATIC_LIST` or its `_FILE`, read again at every sync | none |
| Where a persona's country, language and zone come from | the observed geo of the address; Webshare's country only until a lookup answers | what the entry declares; a lookup only verifies it | the host's own lookup, or `egress.direct.country`/`locale`/`timezone` |
| Replacement, replaced log, account, traffic stats | yes | no: a proxy that keeps failing waits until the list is edited | no |
| Echo the probes read | Webshare's own | the public echo, or the operator's own | none: nothing is probed |
| Section | [Webshare](#webshare) | [A declared list](#a-declared-list) | [levels-and-egress.md](levels-and-egress.md#the-hosts-own-connection-egressmode-direct) |

`proxies.provider` chooses: `auto` (the default) takes Webshare when its key
is set, else the declared list when one is set, else none; `webshare`,
`static` and `none` name one. `kage config check` reports what is in force
under `proxies.provider`, what it can do (`proxies.capabilities`,
`proxies.unsupported`), the endpoints the probes read
(`proxies.health_endpoints`) and what deserves a second look
(`proxies.warnings`).

## What runs

`kage serve` runs two loops when a proxy provider is configured:

- **Upkeep** syncs the pool with the provider's list at start and every
  `proxies.sync_interval` (10 min), reads the provider account when the
  provider keeps one, ends backoffs that ran out, and requests and follows
  replacements when the provider makes them.
- **Health checks** probe every proxy on its own schedule (5 min, give or
  take a quarter), read the host's own public address at start and hourly,
  and look up reputations once a day when a key is set.

Without a provider the pool of a new installation stays empty, neither loop
runs, and nothing is fetched through a persona: under `egress.mode: proxies`
(the default) Kage never connects to a target from the host's own address.
When the provider goes missing from the configuration later, the proxies
already stored are still health-checked, so a leak is still seen, but the
pool is no longer synced and nothing is replaced; the log says so at start.

Under `egress.mode: direct` the host's own connection carries everything
and the pool is idle: neither loop runs, nothing is synced, probed or used,
`kage proxy health` and the admin probe refuse with "the pool is idle under
egress.mode direct", and the pool's personas wait (below, "The host's own
rows", and [levels-and-egress.md](levels-and-egress.md)).

The `kage proxy` commands work on the same database and may run next to the
server. Each prints a table, or one JSON document with `--json`; logs go to
stderr. `kage proxy list` and `kage proxy health` end with the provider and
what it does not offer, for example
`provider: static (lists only; replacement, replaced_log, account and stats unsupported)`.

## What a provider can do

Every provider lists its proxies. The rest is optional, and a provider says
what it offers (`ProxyProvider::capabilities`, spec §7.1):

| Operation | Webshare | Declared list | Without it |
|---|---|---|---|
| `replacement` | yes | no | a proxy due for replacement stays in backoff, probed at the cap, until its address answers again or the operator edits the list; the event `replacement_unsupported` (info) says so once per episode; `kage proxy replace` and the admin endpoint answer `unsupported` |
| `replaced_log` | yes | no | a proxy missing from the list is retired after `proxies.missing_syncs` syncs only |
| `account` | yes | no | no plan, quota or term is read; the replacement budget is counted per UTC month; `kage proxy list` prints no account line |
| `stats` | yes | no | the bandwidth budget is judged on Kage's own count only; the vendor may bill more (its own traffic, other tools on the same plan) |
| an echo of its own | `https://ipv4.webshare.io/` | none | the probes read the public echo, or the operator's own (below) |

Another vendor's API (replacement, account) is a provider module of its own:
`CONTRIBUTING.md`, "Adding a proxy provider".

## Webshare

1. Create an API key in the Webshare dashboard and set
   `KAGE__WEBSHARE__API_KEY` (or `KAGE__WEBSHARE__API_KEY_FILE`). With the
   compose file, put `KAGE_WEBSHARE_API_KEY=...` into `deploy/.env`.
2. **Switch off Webshare's automatic replacement.** In the dashboard, under
   the proxy settings, turn off all four: replace invalid proxies, replace
   proxies with low country confidence, replace proxies out of rotation,
   replace proxies failing site checks. Otherwise Webshare swaps an address
   whenever it sees fit, and the persona that lived on it is lost without
   Kage having decided it. Kage only reads these settings: while one is on,
   `kage proxy list --sync` says so and the alert
   `provider_auto_replace_enabled` is raised. One setting is accepted by
   default and raises nothing, `auto_replace_invalid_proxies`: Webshare
   then replaces only an address its own checks find dead, which Kage
   could not use anyway (`webshare.accepted_auto_replace`; an empty list
   accepts none). A swap made for an accepted reason is recorded as
   `provider_replaced_proxy` at info; the old proxy is retired with its
   persona and the new address gets a new persona, exactly as when Kage
   replaces one.
3. If the account has more than one plan, set `webshare.plan_id`.
4. Optional: an AbuseIPDB key in `KAGE__REPUTATION__ABUSEIPDB_KEY`
   (`KAGE_ABUSEIPDB_KEY` in `deploy/.env`) switches the daily reputation
   lookups on. The free tier allows 1,000 lookups a day; a pool of 20 uses
   20.

The client keeps its own request rate below Webshare's limits (240 a minute
in general, 60 a minute on the list endpoints) and waits as long as a 429
says. A persona on a Webshare proxy takes its country, language and zone
from where the geo lookup places the address; Webshare's own country is the
fallback until a lookup answers, and a lookup that disagrees with it twice
is `proxy_geo_mismatch` (the persona goes by the lookup).

## A declared list

The way to run Kage on the proxies of any vendor. The operator lists the
proxies and **declares** where each one's exit is: its persona is made from
that declaration, and a geo lookup only verifies it.

### The entry

The YAML form covers everything:

```yaml
proxies:
  - id: vienna-1                 # optional; host:port when not given
    host: 198.51.100.10
    port: 8080
    username: user
    password: "pa:ss"            # quoted: it holds a colon
    country: AT                  # ISO 3166-1 alpha-2; needed for a persona
    locale: de-AT                # optional, BCP 47: language-REGION, REGION = country
    timezone: Europe/Vienna      # optional, IANA
    city: Vienna                 # optional, informational
  - id: paris-1
    url: http://user:pa%3Ass@198.51.100.11:8080   # instead of host, port, username and password
    country: FR
```

The line form is Webshare's list download with Kage's two optional fields,
one proxy per line, `#` for a comment:

```text
198.51.100.10:8080:username:password
198.51.100.11:8080:username:password:DE
198.51.100.12:8080:username:password:CH:fr-CH
```

- **`country`** is the claim a persona is made from. **An entry without one
  gets no persona and serves as a plain exit only** (level 0, below);
  `kage serve` warns about such entries at start and `kage config check`
  lists them (`proxies.plain_exits`).
- **`locale`** overrides the country's primary language (Swiss French:
  `CH` with `fr-CH`). It needs `country`, and its region must be that
  country.
- **`timezone`** wins over the lookup's zone; without it the zone is the
  lookup's when the lookup agrees on the country, else the country's main
  zone. Name it for a country of several zones (the United States,
  Canada, Spain's Canaries, Portugal's Azores).
- **`city`** is shown and stored, never compared and never a position:
  vendors and lookup services name cities differently.
- A persona's languages are the list its browser ships for the declared
  language (Chrome in Austria `de-DE, de, en-US, en`, Firefox
  `de, en-US, en`; `docs/runbooks/personas.md`).
- What no persona can be made from is refused at load, at
  `kage config check`, and when the file changes (below), naming the entry
  and never its content: a country the browser tables do not know without
  a locale whose language they know (the known countries are listed in the
  message: AT, BE, CA, CH, DE, ES, FR, GB, IE, IT, LU, NL, PL, PT, RO, SE,
  US; a locale may be in de, fr, it, en, nl, es, pl, pt, ro or sv), a
  locale without a country or in another region, a zone that is no IANA
  zone, a `url` that is not `http` (SOCKS5 is not supported), and two
  entries with the same proxy and login, which would put two personas on
  one address.
- **A gateway** (one host and port, the exit chosen by the user name or the
  password) lists many entries on one `host:port`. Their ids would collide:
  give each an `id`, in the YAML form. Two entries that turn out to leave
  by one address are found by the probes (below, `proxy_exit_shared`).

### Giving the list, and changing it

The list holds passwords: it is a secret, set in
`KAGE__PROXIES__STATIC_LIST` or in a file named by
`KAGE__PROXIES__STATIC_LIST_FILE`, never in the configuration file. With
the shipped compose file, name the file in `deploy/.env`
(`KAGE_STATIC_LIST_FILE=<path>`, absolute or relative to `deploy/`): the
`kage` service mounts it read-only and is told where. Let only the
container's user read it: `sudo chown 65532 <file> && sudo chmod 0400
<file>`.

- **A file is read again at every sync** (`proxies.sync_interval`, 10 min)
  and at once by `kage proxy list --sync`. A new entry is adopted (`new`,
  probed, a persona); an entry taken out is retired after
  `proxies.missing_syncs` (3) syncs; changed credentials are sealed again;
  an `id` at another address is a new proxy.
- **An edit that does not parse changes nothing**: the last good list stays
  in force and `static_list_invalid` (warning) names the entry and the rule.
  Mend the file; the next sync takes it.
- A list in the variable needs a restart:
  `docker compose --file deploy/compose.yaml up --detach --wait kage`.
- A list that came back empty or shorter than half the pool retires
  nothing on its word (`provider_list_suspect`): take more than half out in
  two edits.

### Verification by the lookup

Where a geo lookup is possible (`proxies.health.geo_url`, on by default,
through the proxy), Kage looks the exit address up at the first probe, when
it changes, and once a day. A lookup that agrees on the country gives the
persona its zone (unless one is declared) and a position near it. **Two
lookups in a row that place the address in another country are
`proxy_geo_mismatch`**: the proxy is `degraded` and stays usable, and the
persona keeps its declared identity. The alert says what a site with a geo
database would see: a browser that claims the declared country from an
address the lookup places elsewhere. To resolve it:

- the declaration is wrong: correct it (below);
- the address is not worth a persona there: leave its `country` out, and
  it serves as a plain exit;
- the lookup service is wrong for your vendor's addresses: switch the
  lookup off with `proxies.health.geo_url: null` (the key is the switch;
  nothing is verified then).

### Correcting a declaration

- **Another country, or another language** makes a new persona: the old one
  is retired (`declaration_changed`), its profile archived, and a new one
  is generated from the corrected declaration and warms up. Its cookies and
  history were made under the wrong identity, and a new device behind the
  same address is what a household looks like.
- **Another zone or city** in the same country and language moves the
  persona: its zone follows at once (`persona_geo_updated`, info).
- **No country any more**: the persona is retired (`undeclared`) and the
  proxy is a plain exit.

### Plain exits

A declared entry without a country carries no persona and never reads any
site as a person, so level 0 prefers it: a site whose plain traffic has no
exit yet gets one of the usable plain exits (a draw seeded by the site, kept
for the site from then on), and robots.txt and sitemaps at level 0 go the
same way. Only when the pool has none does a site's level 0 leave by its
scout's address (spec §25). While a site's plain exit cannot carry traffic
its level 0 waits or is refused (`plain_exit_unavailable`), never sent from
another address. An operator who wants level 0 at scale dedicates a few
entries without a country to it. Do not give a plain exit a country later:
it would read as a person the sites it served at level 0 (the event
`plain_exit_released` says how many sites lost it); add a new entry
instead.

### The echo, and who sees the pool

A declared list has no echo of its own. Unless `proxies.health.echo_url`
names one, **every probe reads the public echo
`https://checkip.amazonaws.com/` through each proxy, so its operator sees
every proxy address on the probe's schedule** (5 min each), and the host's
own address is read from `https://api.ipify.org/` (another operator, so
nobody outside sees both the host and the pool; `proxies.health.host_ip_url`).
`kage config check` warns while the public echo is in force. The private
choice is the operator's own echo, `kage echo serve`
([fingerprint-echo.md](fingerprint-echo.md)), behind a DNS name and a
certificate: set `proxies.health.echo_url` (and `host_ip_url`, read from the
host) to it. The probe reads an address as plain text, or JSON with it
under `ip` (Kage's echo writes `peer`), with or without the port.

## First start

```bash
kage migrate
```

```bash
kage proxy list --sync
```

```bash
kage proxy health
```

`list --sync` adopts the provider's list: every proxy is `new`. `health`
probes every proxy once and prints the result, latency, exit address and
country; healthy proxies become `healthy`. It ends with how many answered
and how many did not (`4 checked: 3 live, 1 dead`; `live` and `dead` in the
JSON), the pool's line (`pool: 4 live, 3 usable …`, where `live` means not
retired) and the provider line. From then on `kage serve` keeps the pool by
itself. A declared list needs no API: `kage proxy health` on it says which
proxies are live and which are dead, from the echo.

Next to the total latency, `health` shows how long the proxy took to open
the tunnel (`TUNNEL`: TCP to the proxy plus its CONNECT) and how long the
TLS handshake through it took (`TLS`). Every new connection a persona opens
pays the first two again, so a proxy with a slow tunnel is slow on every
first request to a host, however quickly it answers afterwards.

## Adding proxies

More proxies of the same provider need nothing of Kage: buy them at
Webshare and the next sync adopts them (every `proxies.sync_interval`,
10 min; `kage proxy list --sync` at once). Each arrives `new`, is probed,
becomes `healthy`, and the server makes it a persona of its own, drawn
under `personas.mix` from the country the proxy was observed in (or
declared in), which warms up over its first hours
([personas.md](personas.md)). Check the bandwidth budget first:
`bandwidth.monthly_cap_gb` is the plan's, not the pool's, and more personas
spend it faster.

With a declared list in a file, add the entries to the file; the next sync
takes them. A list in the variable needs a restart.

## States

```
new ──ok──▶ healthy ──failure──▶ degraded ──3 failures──▶ backoff ──expiry──▶ degraded
              ▲                      │
              └───────── ok ─────────┘
backoff ──3 cycles and 6 h of failing, or bad reputation──▶ replacing ──done──▶ retired
```

- `healthy` and `degraded` proxies are usable. `degraded` means a check
  failed, the country does not match, the reputation is bad, or the proxy
  leaves by an older proxy's address.
- A check fails only when both echoes fail through the proxy. When the
  confirmation echo is the echo in force (a declared list with the
  defaults), the confirmation is a second read on a fresh tunnel.
- `backoff` lasts 1 min, doubling with every cycle in a row up to 1 h, each
  jittered into its upper half. When it ends the proxy is `degraded` and is
  probed at once: one more failure sends it back.
- A proxy is replaced only after three backoff cycles in a row **and** six
  hours of failing without a single passing check (`proxies.replace_after`),
  and only by a provider that offers replacement. A short outage never
  costs a replacement.
- When more than half of a pool of four or more fails at once, the failure
  counters freeze and `pool_outage_suspected` is raised: the cause is the
  host's network, the provider's gateway or the echo, not the addresses.

## Replacing a proxy

Automatic replacements and `kage proxy replace` share one budget:
`proxies.replacements_per_month` (5) per budget window. The window is the
provider's subscription term when the account reports one (Webshare counts
its own replacement quota per 30-day term, not per calendar month), else
the UTC calendar month; `kage proxy list` names it ("replacements this
term (since 2026-09-21)"). The budget is checked in the database, so the
server and the CLI cannot overspend it together.

```bash
kage proxy replace <proxy-id>
```

The command asks Webshare for a proxy in the same country, waits until
Webshare is done (`--no-wait` returns at once and leaves the follow-up to the
server), and adopts the new address. It exits non-zero when the proxy was not
replaced, and says why:

| Message | What to do |
|---|---|
| this month's replacement budget is used up | Wait for the next month, or raise `proxies.replacements_per_month` for this one command: `KAGE__PROXIES__REPLACEMENTS_PER_MONTH=6 kage proxy replace <id>` |
| the provider's plan has no replacements left | Buy replacements at Webshare, or wait for the plan to renew |
| the provider has no proxy to offer in that country | Nothing is sent: Kage does not trade the pool's country mix for a faster replacement. Try later |
| the provider does not offer replacement | A declared list: edit the list. The host's own row: nobody replaces the host |
| the proxy is `replacing` or `retired` | A replacement is already under way, or done |
| the provider refused: ... | Webshare's own reason follows. One replacement runs at a time: wait for the pending one |

The replaced proxy is `retired` and its row stays as history. Its persona
is retired with it: one persona lives on one address and never moves to
another, so its profile is archived and deleted after
`personas.retention_days`, and its cookies and sessions end. The new
address arrives as `new` and gets a new persona that knows nothing of the
old one. A site therefore sees one household leave and another arrive,
never one browser change its address.

A proxy whose persona is a site's scout is that site's plain exit when the
pool has no plain exit of its own
([levels-and-egress.md](levels-and-egress.md#under-the-pool-egressmode-proxies)).
While the proxy is in backoff or being replaced, level 0 for those sites
is answered `plain_exit_unavailable` and never sent from another address;
after a replacement the site gets a new scout, and with it a new plain
exit.

## The host's own rows (`egress.mode: direct`)

Under `direct` each host persona (`egress.direct.personas`, 1 to 4) has a
row of its own in the pool so that it keeps its one "proxy" like any
persona: provider `host`, reference `host-1` to `host-4`, address
`host.invalid`, `healthy` from the start. `kage proxy list` shows them
beside the idle pool. Nothing is relayed to that address: the forwarder
sends the persona's traffic out of the host's own connection.

- A host row is never synced, probed, leak-checked, counted in the outage
  share or the capacity, or replaced: `kage proxy replace` answers that
  the provider does not offer replacement.
- Its place follows the same rule as a declared list's entry. Declared
  values win: `egress.direct.country`, `locale` and `timezone`, written on
  the row at every start; a lookup of the host's own address
  (`proxies.health.host_ip_url`, by default `https://api.ipify.org/`, and
  `proxies.health.geo_url`, read directly, at start and once per
  `proxies.health.geo_interval`) then only verifies them, two that disagree
  in a row being `geo_mismatch`. Without a declared country the lookup
  places the rows. A declaration changed between two starts is corrected
  as a list's entry is.
- The pool's leak flag (`egress_unsafe`) and the bandwidth budget do not
  hold the host's connection.
- When the mode goes back to `proxies`, the host's rows are retired at
  the next start, with their personas (`egress_mode`).

## Alerts

Alerts are logged and stored in `health_events`. Each is raised once per
episode, not once per check. What is standing is kept in memory, so a
restart, and a `kage proxy` command, raise a still-standing alert once more.

| Alert | Severity | Meaning and action |
|---|---|---|
| `egress_unsafe` | critical | A proxy let the host's own address through. See the next section |
| `proxy_leak` | critical | The proxy it happened on; it is parked in backoff |
| `pool_outage_suspected` | critical | More than half of the pool fails at once. Check the host's network, the provider's status, and the echo endpoints. Counters are frozen; nothing is replaced |
| `provider_list_suspect` | critical | The provider's list came back empty or shorter than half the pool. Nothing was retired. Check the account, or take entries out of a declared list in two edits |
| `replacement_budget_exhausted` | critical | A proxy qualifies for replacement but the monthly budget, or the plan's quota, is used up. It stays in backoff and recovers by itself if the address comes back |
| `replacement_failed` | warning | A replacement was refused, failed at the provider or left no trace: the proxy is back in backoff and the budget slot is free. Also raised when a replacement has been pending at the provider for over an hour; that one is still waited for |
| `pool_capacity_low` | warning | A quarter or more of the pool is in backoff or being replaced |
| `proxy_geo_mismatch` | warning | Two geo lookups in a row place the exit address in another country than the provider states or the operator declared. The proxy stays usable, `degraded`. A Webshare persona goes by the observed country; a declared one keeps its declaration (see "Verification by the lookup") |
| `proxy_exit_shared` | warning | A proxy's probe saw the exit address of an older proxy: two personas would share one address (decision D6). The later proxy carries nothing, neither a persona nor level 0, until it leaves by an address of its own: fix the list |
| `proxy_reputation_bad` | warning | AbuseIPDB scored the address at or above `reputation.bad_score` twice in a row. It is replaced when the provider and the budget allow, else it stays usable as `degraded` |
| `reputation_usage_type_suspicious` | warning | AbuseIPDB classifies a residential address as data center or hosting space. Never replaced automatically: decide, and use `kage proxy replace` |
| `provider_replaced_proxy` | warning (info for a reason `webshare.accepted_auto_replace` covers) | Webshare replaced a proxy by itself. Switch its automatic replacement off, unless the reason is an accepted one |
| `provider_auto_replace_enabled` | warning | One of Webshare's automatic replacement settings is on |
| `provider_account` | warning | The subscription is paused or throttled, or the plan has no replacements left |
| `provider_sync_failed` | warning | The provider's list could not be read. The pool keeps what it has |
| `static_list_invalid` | warning | The declared list's file changed and does not parse; the last good list stays in force. The alert names the entry and the rule |
| `host_ip_unknown` | warning | The host's own address could not be read, so the leak check cannot run |

Events that are no alert: `replacement_unsupported` (info: a proxy
qualifies for a replacement its provider does not offer),
`proxy_declaration_changed` (info: a host row took another declared
place), `plain_exit_released` (warning: a plain exit was given a country and
its sites chose another exit), `proxy_updated` (info: the list changed
credentials, country, locale, zone or city).

## A leak: `egress_unsafe`

A probe that sees the host's own address as the exit address means requests
through that proxy left the host directly. That is a fault on the path, not
in one address, so one leak makes the whole pool unsafe: the scheduler must
not dispatch anything while the flag is set.

```bash
kage proxy egress
```

lists the proxies with a leak on record. Find the cause first: a proxy
variable in the environment, a network change on the host, a provider-side
routing change. Then either probe the proxy again,

```bash
kage proxy health --id <proxy-id>
```

which clears the flag when the exit address is distinct again, or, when the
proxy is gone and cannot be probed, clear it by hand:

```bash
kage proxy egress --clear
```

Retiring or replacing the proxy does not clear the flag. A leak never counts
toward a replacement.

## Credentials at rest

Proxy user names and passwords are stored encrypted (XChaCha20-Poly1305)
under `KAGE__SECRETS__MASTER_KEY`: 32 random bytes written as 64 hexadecimal
characters. `just setup` generates one; `openssl rand -hex 32` does too.

- `kage proxy list` never shows a credential. `--reveal-username` decrypts
  and shows the user name, and writes an audit event.
- **Rotating the master key:** set the new key and restart. The stored
  credentials no longer open, so no proxy can be probed or used until the
  next sync with the provider (at start, or `kage proxy list --sync`), which
  seals the provider's credentials again under the new key. With a declared
  list the list itself is the source.
- **Losing the master key** is the same procedure: the provider, or the
  list, still has the credentials.

## What the health checks contact

| Endpoint | Key | Default | Seen from | How often |
|---|---|---|---|---|
| reachability echo | `proxies.health.echo_url` | Webshare: `https://ipv4.webshare.io/`; a declared list: `https://checkip.amazonaws.com/` | each proxy | every probe |
| confirmation echo | `proxies.health.confirm_url` | `https://checkip.amazonaws.com/` | a proxy whose first echo failed | once before a failure counts |
| geo lookup | `proxies.health.geo_url` | `https://ipinfo.io/json` | each proxy | first check, exit address change, once a day |
| host address echo | `proxies.health.host_ip_url` | Webshare: `https://ipv4.webshare.io/`; a declared list, and always under `direct`: `https://api.ipify.org/` | the host, directly | start, then hourly; under `egress.mode: direct` start, then once per `geo_interval` |
| geo lookup of the host | `proxies.health.geo_url` | `https://ipinfo.io/json` | the host, directly, only under `egress.mode: direct` | start, then once per `geo_interval`: the place of the host's personas |
| reputation | `reputation.base_url` | `https://api.abuseipdb.com/` | the host, directly | once a day per proxy, only with a key |

Webshare's echo belongs to the provider, who sees every connection anyway,
so its probes add no observer. A declared list's probes show every proxy
address to the public echo's operator (above, "The echo, and who sees the
pool"). Probes present the TLS, HTTP/2 and header fingerprint of the
persona behind the proxy, or of `proxies.health.profile` (the newest
Chrome profile of the pinned wreq-util) while it has none. Under
`egress.mode: proxies` the requests marked "the host, directly" are the
only ones Kage makes without a proxy, besides webhooks, and none goes to a
target. Under `direct` the pool is not probed at all, and every request
leaves from the host. No health traffic ever goes to a target site.

## The live check

```bash
KAGE_LIVE=1 KAGE__WEBSHARE__API_KEY=... KAGE__SECRETS__MASTER_KEY=... just test-live
```

starts a throwaway Postgres, syncs the real proxy list and probes every
proxy once. It passes when every proxy answers with a latency and a country.
It contacts only the endpoints in the table above. Set
`KAGE_LIVE_EXPECT_PROXIES` when the plan does not have 20 proxies.
