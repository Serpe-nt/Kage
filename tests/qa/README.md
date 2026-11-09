# The QA harness

The closed fixture world of the end-to-end tests, made into stacks that a QA
round drives by hand or by agents: round one (`docs/qa/2026-10-30.md`) built
it, round two (`docs/qa/2026-11-03.md`) added the worlds below. Nothing here
runs in CI; `deploy/compose.e2e.yaml` is unchanged by it, and every world is a
compose overlay on top of it.

## The rule

Nothing leaves the world. The harness runs inside a privileged development
container with a Docker daemon of its own, and before the first stack starts
`stack.sh fw-on` drops every packet that the nested containers or the
container itself send out of its `eth0`. `stack.sh fw` shows the counters; a
drop is something to explain. Every URL a CLI call or a configuration names is
a world name: a default URL of the product points at a real service. Never
`KAGE_LIVE=1`, never a real key, never a real proxy.

## The container

`container/` holds what round two ran: `bootstrap.sh` (a non-root `dev` user,
the volumes' owners, the nested daemon through `fix-dockerd.sh` and
`start-dockerd.sh`) and `prepare.sh` (clones the commit under test from a git
bundle at `/out/qa.bundle` into `/target/qa/kage`, builds the three images,
the CLI and the node projects). The image is the development image of the
milestones (`kage-dev`); the container runs with `--privileged --init`, this
directory mounted at `/scripts/qa`, the `container/` scripts at `/scripts`,
and a log directory at `/out`. Pull every image a world needs (Postgres, and
for `infra/timers/obs.yaml` the observability images at their digests) before
`fw-on`.

## The harness

- `stack.sh` starts one world per name under the compose project `qa-<name>`
  on a port of its own and waits for its personas (`QA_PERSONAS`, default 3);
  `--overlay=FILE` adds a compose file at `up`. Its head lists the commands:
  `up`, `down`, `restart`, `url`, `admin`, `key`, `call`, `cli`, `dc`, `sql`,
  `get`, `responder`, `policies`, `add-domains`, `fast`, `logs`, `fw`.
- `compose.qa.yaml` adds the programmable `responder.mjs` (any status,
  redirects, robots.txt, sitemaps, answers in turn) as `qa.kage.test:8091` and
  `other.qa.test:8091`; `fast-domains.yaml` puts the world's names on a fast
  pace with no sleeping hours.
- `sweep.sh` runs a round's `repro.sh` scripts (each ends `REPRODUCED` or
  `NOT-REPRODUCED`) on a shared stack, on stacks of their own, or each on a
  fresh one; `verify2.sh` is round two's reproduction of its own findings;
  `verify.sh` and `fresh.sh` are round one's.
- `cli/k.sh` runs the CLI of the commit under test with the shipped defaults
  and throwaway secrets (made at run time), for `kage config check`;
  `ssrf/lib.mjs`, `extract/lib.mjs`, `crawl/h.mjs` are helpers round one's
  repro scripts import.

## The worlds of round two (`infra/`)

| Folder | What it adds | How a stack uses it |
|---|---|---|
| `tls/` | `https`: a private CA and server certificates made at run time by `mkcerts.sh` (good, expired, wrong name, unknown CA, self-signed; keys stay in `certs/`, ignored by git), `tlsworld.mjs` (https on 443 by SNI, the responder's API on 80, a TLS front of the fixture site), the product's own `kage echo serve` on 8443, the proxies' allow lists | `bash infra/tls/mkcerts.sh` once, `stack.sh up NAME --overlay=/scripts/qa/infra/tls/compose.tls.yaml`, then `infra/tls/trust.sh NAME` writes the CA into every persona's profile (Chromium and Camoufox NSS databases on the personas volume); `compose.tls-warm.yaml` and `up-warm.sh` for warm-up on the world's pages, `compose.tls-hooks.yaml` for https webhooks; `compose.tls-level0.yaml` names the CA in `http.ca_file` (F191) so that level 0 reads the world's https sites, and `level0.sh` brings up a stack of its own and checks level 0 over https end to end (a read page, refused certificates as `connection_failed`, a site's 407, robots.txt kept and unreachable, F90's 304 observed) |
| `ssrf/` | `dns.mjs`, a programmable DNS server (answers in turn with TTL 0, per asking address, CNAMEs; forwards only `*.test`, `*.invalid` and one-label names), a second responder on a "public" network, `udpcatch.mjs` and `udpwatch.sh` for UDP a browser might send around the proxy, `webrtc.html` | `infra/ssrf/own-stack.sh NAME NET`, or `--overlay=/scripts/qa/infra/ssrf/overlay-dns.yaml SSRF_NET=11.66.N`; read the overlay's head on its address space |
| `pool/` | five proxies with exits in three /24s and four ASNs (`echo.mjs` replaces the world's echo with declared exits), a fake Webshare API (`webshare.mjs`: list, replace and its status, the replaced log, subscription, plan, config, stats; `/__drop`, `/__fail` to act as the provider), a second kage server on one database (`two-servers.sh`), proxies that answer 407 or refuse a target (`proxy-faults.yaml`) | `stack.sh up NAME --overlay=/scripts/qa/infra/pool/five-proxies.yaml QA_PERSONAS=5`, plus `webshare.yaml` (and `webshare-auto.yaml`) for the provider; `two-servers.sh NAME up` after `up` |
| `http/` | the responder on two ports and a third name (`multi.mjs`, `ports.yaml`), 40 more names for robots.txt cases (`extra.sh`) | `--overlay=/scripts/qa/infra/http/ports.yaml`, then `infra/http/extra.sh NAME` |
| `timers/` | hooks that fail every delivery and extra names for pacing runs (`real.yaml`), the observability profile with this harness's paths (`obs.yaml`) | `--overlay=` either; no shortened clock |
| `flows/` | eight more sites `s1..s8.qa.test` for flows across sites, with their trailing-dot spellings (`mk-extra.sh`), their domain rules and robots files (`sites.sh`), helpers (`lib.sh`) | `infra/flows/mk-extra.sh NAME`, then `infra/flows/sites.sh NAME`; recreating the proxies can hand them each other's addresses (round two, timers-04): prefer a fresh stack |
| `left/` | the leftovers' worlds: sites `dc1.test`, `dc2.test` and a name every proxy refuses with 403 (`repro.yaml`, `refuse.yaml`), more responder names (`names.yaml`), five proxies (`five.yaml`), a `fast`-like pacing profile with `domain_concurrency: 1` (`single.yaml`), their domain rules | `--overlay=/scripts/qa/infra/left/repro.yaml` at `up`; append `single.yaml` to the stack's `pacing-profiles.yaml` (`stack.sh policies NAME`), then `stack.sh add-domains NAME < /scripts/qa/infra/left/repro-domains.yaml` |

## The worlds of round three (`infra/`)

Round three (`docs/qa/2026-11-07.md`) ran in a container `kage-qa3` the same
way, with the firewall of `container/fw-on.sh`: an NFLOG rule before each DROP
copies every dropped packet to `/out/qa3/fw-drops-N.pcap` (`tcpdump -i
nflog:7`; the LOG target writes nothing from a nested network namespace).
`container/pull.sh` pulls every image the worlds need before it goes on;
`build-202.sh` makes the 2.0.2 tree (`/target/qa/kage-202`) and its Firefox
image for the upgrade; `rebuild-sidecars.sh` moves the clone to a newer `main`
from a bundle and rebuilds (firewall off for the build, on again after).

| Folder | What it adds | How a stack uses it |
|---|---|---|
| `level0/` | `tlsworld0.mjs` (one state on 443, 4443, 80 and 8081; 16 more https sites, TLS fronts for plain services, an `/__ip` echo), `mkcerts0.sh` (its certificate and bad CA files, keys in `tls/certs/`), `compose.level0.yaml`, `compose.l0-webshare.yaml` with `webshare0.mjs` (the fake Webshare behind TLS), `fast-l0.yaml` | after `tls/mkcerts.sh` and `level0/mkcerts0.sh`: `--overlay=` the tls world, `compose.tls-level0.yaml` (F191's `http.ca_file`) and `compose.level0.yaml` |
| `lock/` | `lib.sh` (who holds the server lock; a cut between `kage` and Postgres in kage's network namespace that keeps the addresses), `second.sh` (a `kage2` second server on the stack's database, no restart) | source `lib.sh` beside a stack that is up |
| `ops/` | the upgrade's world (`upgrade-world.yaml`, `upgrade-domains.yaml`, `capped-profile.yaml`, `r02-domains.yaml`) and the first start's (`fs-world.yaml`) | `QA_REPO=/target/qa/kage-202` with the 2.0.2 images for the old side, then main's on the same volumes |
| `api/` | `nopool.sh NAME up\|url\|down`: a second server with no proxy pool, on a fresh database of its own, beside a stack that is up | api-09 |
| `browser/` | `keys.html` (an input that writes every key event and the field's value into the document), `keys-analyze.mjs` | served by the responder; browser-06 |
| `bgcalls/` | `sink.mjs` (a stand-in on 443 and 80 that logs the TLS server name or request line of each connection and answers nothing), `overlay-sink.yaml` (a DNS that answers Google's and Mozilla's zones with the sink, on a local bridge in assigned public space as round two's ssrf world: safe only with the egress DROP; read its head), `sink.sh`, `hosts.sh`, `exp.sh` | `--overlay=/scripts/qa/infra/bgcalls/overlay-sink.yaml BG_NET=11.66.77` with `KAGE_E2E_EGRESS_MODE=direct`; bgcalls-01, bgcalls-02 |
| `flows/names3.sh` | writes `names3.yaml`, an overlay given at `up` that adds `s1..s8` and `n1..n6.qa.test` without recreating the proxies | `--overlay=` its output |

`sweep3.sh` runs rounds one's and two's scripts (`r1-shared`, `r1-own`,
`r2-fresh`, `r2-own`), `verify3.sh` reproduces round three's findings,
`retry3.sh N ID...` tries a race up to N times on fresh stacks, and
`dnscheck.sh` counts the browsers' background lookups that leave under the
pool and under `direct`.

## Round four, part A: the core (`infra/`)

Round four's core part (`docs/qa/2026-11-09-core.md`) ran in a container
`kage-qa4-core`, started from the host by `container/host-up4.sh WORKTREE
SCRATCH` (the worktree read-only at `/work`, `SCRATCH/scripts` at
`/scripts`, `SCRATCH/out` at `/out`). `container/pull4.sh` pulls Postgres,
the observability images and k6 at round three's digests; `prepare4.sh`
clones the commit from `/out/qa.bundle` into `/target/qa/kage` and builds
only the three images: the CLI is the `kage` image's own binary, run with no
network by `cli/k4.sh`, which `prepare4.sh` installs where round one's
scripts look for a debug build (`/target/devtarget/debug/kage`), so no
cargo target is filled. `fw-on4.sh` / `fw-off4.sh` are round three's
firewall with the round in a variable (capture `/out/qa4/fw-drops-N.pcap`).
A sweep that runs round three's ops-02 and ops-03 needs the 2.0.2 images
pulled (and built, for Firefox) before the firewall.

| Folder | What it adds | How a stack uses it |
|---|---|---|
| `sched/` | `names.sh` and `names4.sh` write `names3.yaml` / `names4.yaml` beside themselves: `s1..s8`, `n1..n6.qa.test` and (names4) `sc1..sc8.test`, each a registrable domain of its own, fresh sites for robots scouts; `fast-names.yaml`, `fast-sc.yaml` (their domain rules, `sc*` with `robots: enforce`) | `--overlay=/scripts/qa/infra/sched/names4.yaml` at `up`, then `stack.sh add-domains NAME < .../fast-names.yaml` |
| `guard/` | `rawproxy.mjs` (the world's proxy that logs each raw request target, `Host` and header names at `GET /__raw`, and answers listed targets with a status: `--answer host:port=403,...`, `--challenge`), `guard.yaml` (the three proxies run it; `deny403/407/502.qa.test`; the responder on 8091 and 8092), `guard-domains.yaml`, `blackhole.pl` (a proxy that drops SYNs or accepts and stays silent), `hooks25.yaml` (a receiver on port 25) | `--overlay=/scripts/qa/infra/guard/guard.yaml` at `up`, then `add-domains < guard-domains.yaml` |
| `obs/` | the collector with a `detailed` debug exporter (`collector-debug.yaml`, mounted by `debug.yaml`), `exits3.yaml` (settable exits for the three default proxies), `up.sh` (a stack with the observability profile and a Grafana password made at run time), `obs.sh` (Prometheus, Jaeger, Grafana and `/metrics` helpers), `panels.mjs` (runs every dashboard query) | `infra/obs/up.sh NAME [GRAFANA_PORT]` |

`sweep4core.sh` runs the core's rows of rounds one to three (`list`,
`r1-shared`, `r1-own`, `r2-fresh`, `r2-own`, `r3`; round three's flows-04 as
round F's copy pinned to Chromium, from the evidence), `verify4.sh`
reproduces a round's findings each on a fresh stack or its own, and
`retry4.sh N ID...` tries a race up to N times.

## Round four, part B: the browsers (`infra/browser/`)

Round four's browser part (`docs/qa/2026-11-09-browsers.md`) ran in a
container `kage-qa4-browsers` with round three's container scripts, which
take the round where part A's `*4.sh` copies hard-code it:
`fw-on.sh` / `fw-off.sh` read `QA_FW_DIR` and `QA_FW_TAG` (default
`/out/qa3`, `QA3`) and refuse without `tcpdump`; `pull.sh` leaves out the
release images with `QA_PULL_RELEASES=0`; `prepare.sh` builds the three
images only with `QA_PREPARE=images`.

| Script | What it does |
|---|---|
| `keys4.html` | records every key, input, composition, pointer and wheel event into the page (`?options=N` fills a select, `?focus=1` focuses its input) |
| `keys4-analyze.mjs` | reads that page: lost characters, holds and intervals by kind (plain, lent, dead), keys with no `code`, `"\u0000"` keys, capitals without Shift, the code of each character, typos against the physical keyboard |
| `type4.sh STACK ENGINE TEXT_I [TEXT_T]` | a level 3 flow types into the page and prints the analysis (`TYPOS`, `HUMAN`, `EXTRA`, `SESSION`) |
| `layout.sh COUNTRY LOCALE [TEXT]` | a direct-mode stack of its own with that declared country and locale; types with each engine |
| `tto.sh STACK ENGINE KIND [BUDGET_MS]` | a step of KIND runs out of its time; the next request's page records what it hears (`CROSS=1`: another key's request, `WAITB`) |
| `p85.sh STACK [PARTS]` | probes next to #85's roads: repeated header lines, a 304 naming other fields, Firefox's kept body (`P85_ONLY`), a flow's bound, hidden links, a 304 with nothing kept, downloads |
| `sel4.sh`, `f235.sh`, `f235p.sh` | a select's keys as a page hears them; a click to a page that never answers (both engines, steps after it, the pause it books) |

`sweep4b.sh` runs this part's rows of rounds one to three (`r1-shared`,
`r1-own`, `r2-fresh`, `r2-own`, `r3-shared`, `r3-own`, `confirm`; `SKIP3`
leaves out round three's ids an unmerged fix covers).

Each finding of a round has its `finding.md` and `repro.sh` in the round's
evidence, outside the repository; a repro names the world it needs on its
second line (`# own-stack: ...`).
