# Runbook: the proxy pool

How the pool gets its proxies, what it does with them, and what an operator
does when it raises an alert. Spec §7 is the contract; this page is the
practice.

## What runs

`kage serve` runs two loops when a proxy provider is configured:

- **Upkeep** syncs the pool with the provider's list at start and every
  `proxies.sync_interval` (10 min), reads the provider account, ends
  backoffs that ran out, and requests and follows replacements.
- **Health checks** probe every proxy on its own schedule (5 min, give or
  take a quarter), read the host's own public address at start and hourly,
  and look up reputations once a day when a key is set.

Without a provider the pool of a new installation stays empty, neither loop
runs, and every fetch fails: Kage never connects to a target directly. When
the provider goes missing from the configuration later, the proxies already
stored are still health-checked, so a leak is still seen, but the pool is no
longer synced and nothing is replaced; the log says so at start.

The `kage proxy` commands work on the same database and may run next to the
server. Each prints a table, or one JSON document with `--json`; logs go to
stderr.

## Choosing the provider

| `proxies.provider` | Provider | Needs |
|---|---|---|
| `auto` (default) | Webshare when its key is set, else the static list when one is set, else none | |
| `webshare` | The Webshare API | `KAGE__WEBSHARE__API_KEY` |
| `static` | A fixed list | `KAGE__PROXIES__STATIC_LIST` |
| `none` | No provider | |

`kage config check` reports which one is in force under `proxies.provider`.

### Webshare

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
says.

### A static list

For tests and for vendors without an API. One proxy per line, the format of
Webshare's own list download, with an optional country:

```text
198.51.100.10:8080:username:password
198.51.100.11:8080:username:password:DE
```

or YAML, which also covers passwords and hosts that contain a colon:

```yaml
proxies:
  - host: 198.51.100.10
    port: 8080
    username: user
    password: "pa:ss"
    country: DE
    city: Berlin
```

The list is a secret: set it in `KAGE__PROXIES__STATIC_LIST`, or name a file
with `KAGE__PROXIES__STATIC_LIST_FILE`. A static proxy cannot be replaced:
one that keeps failing stays in backoff until the list is edited. A proxy
removed from the list is retired after `proxies.missing_syncs` syncs.

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
country; healthy proxies become `healthy`. From then on `kage serve` keeps
the pool by itself.

Next to the total latency, `health` shows how long the proxy took to open
the tunnel (`TUNNEL`: TCP to the proxy plus its CONNECT) and how long the
TLS handshake through it took (`TLS`). Every new connection a persona opens
pays the first two again, so a proxy with a slow tunnel is slow on every
first request to a host, however quickly it answers afterwards.

## States

```
new ──ok──▶ healthy ──failure──▶ degraded ──3 failures──▶ backoff ──expiry──▶ degraded
              ▲                      │
              └───────── ok ─────────┘
backoff ──3 cycles and 6 h of failing, or bad reputation──▶ replacing ──done──▶ retired
```

- `healthy` and `degraded` proxies are usable. `degraded` means a check
  failed, the country does not match, or the reputation is bad.
- A check fails only when both echoes fail through the proxy.
- `backoff` lasts 1 min, doubling with every cycle in a row up to 1 h, each
  jittered into its upper half. When it ends the proxy is `degraded` and is
  probed at once: one more failure sends it back.
- A proxy is replaced only after three backoff cycles in a row **and** six
  hours of failing without a single passing check (`proxies.replace_after`).
  A short outage never costs a replacement.
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
| this provider cannot replace proxies | Static list: edit the list |
| the proxy is `replacing` or `retired` | A replacement is already under way, or done |
| the provider refused: ... | Webshare's own reason follows. One replacement runs at a time: wait for the pending one |

The replaced proxy is `retired` and its row stays as history. The new address
arrives as `new`.

## Alerts

Alerts are logged and stored in `health_events`. Each is raised once per
episode, not once per check. What is standing is kept in memory, so a
restart, and a `kage proxy` command, raise a still-standing alert once more.

| Alert | Severity | Meaning and action |
|---|---|---|
| `egress_unsafe` | critical | A proxy let the host's own address through. See the next section |
| `proxy_leak` | critical | The proxy it happened on; it is parked in backoff |
| `pool_outage_suspected` | critical | More than half of the pool fails at once. Check the host's network, Webshare's status, and the echo endpoints. Counters are frozen; nothing is replaced |
| `provider_list_suspect` | critical | The provider's list came back empty or shorter than half the pool. Nothing was retired. Check the account |
| `replacement_budget_exhausted` | critical | A proxy qualifies for replacement but the monthly budget, or the plan's quota, is used up. It stays in backoff and recovers by itself if the address comes back |
| `replacement_failed` | warning | A replacement was refused, failed at the provider or left no trace: the proxy is back in backoff and the budget slot is free. Also raised when a replacement has been pending at the provider for over an hour; that one is still waited for |
| `pool_capacity_low` | warning | A quarter or more of the pool is in backoff or being replaced |
| `proxy_geo_mismatch` | warning | Two geo lookups in a row place the exit address in another country than the provider states. The proxy stays usable; personas and country filters go by the observed country |
| `proxy_reputation_bad` | warning | AbuseIPDB scored the address at or above `reputation.bad_score` twice in a row. It is replaced when the budget allows, else it stays usable as `degraded` |
| `reputation_usage_type_suspicious` | warning | AbuseIPDB classifies a residential address as data center or hosting space. Never replaced automatically: decide, and use `kage proxy replace` |
| `provider_replaced_proxy` | warning (info for a reason `webshare.accepted_auto_replace` covers) | Webshare replaced a proxy by itself. Switch its automatic replacement off, unless the reason is an accepted one |
| `provider_auto_replace_enabled` | warning | One of Webshare's automatic replacement settings is on |
| `provider_account` | warning | The subscription is paused or throttled, or the plan has no replacements left |
| `provider_sync_failed` | warning | The provider's list could not be read. The pool keeps what it has |
| `host_ip_unknown` | warning | The host's own address could not be read, so the leak check cannot run |

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
  seals the provider's credentials again under the new key. With a static
  list the list itself is the source.
- **Losing the master key** is the same procedure: the provider still has
  the credentials.

## What the health checks contact

| Endpoint | Key | Seen from | How often |
|---|---|---|---|
| `https://ipv4.webshare.io/` | `proxies.health.echo_url` | each proxy | every probe |
| `https://checkip.amazonaws.com/` | `proxies.health.confirm_url` | a proxy whose first echo failed | once before a failure counts |
| `https://ipinfo.io/json` | `proxies.health.geo_url` | each proxy | first check, exit address change, once a day |
| `https://ipv4.webshare.io/` | `proxies.health.host_ip_url` | the host, directly | start, then hourly |
| `https://api.abuseipdb.com/` | `reputation.base_url` | the host, directly | once a day per proxy, only with a key |

The echo every probe reads belongs to the proxy provider, who sees every
connection anyway, so the probes add no observer. Probes present the TLS,
HTTP/2 and header fingerprint of `proxies.health.profile` (the newest Chrome
profile of the pinned wreq-util) until personas bring their own client (M6).
The two requests marked "the host, directly" are the only ones Kage makes
without a proxy, and neither goes to a target.
No health traffic ever goes to a target site.

## The live check

```bash
KAGE_LIVE=1 KAGE__WEBSHARE__API_KEY=... KAGE__SECRETS__MASTER_KEY=... just test-live
```

starts a throwaway Postgres, syncs the real proxy list and probes every
proxy once. It passes when every proxy answers with a latency and a country.
It contacts only the endpoints in the table above. Set
`KAGE_LIVE_EXPECT_PROXIES` when the plan does not have 20 proxies.
