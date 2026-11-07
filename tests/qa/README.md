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

Each finding of a round has its `finding.md` and `repro.sh` in the round's
evidence, outside the repository; a repro names the world it needs on its
second line (`# own-stack: ...`).
