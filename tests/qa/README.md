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

Each finding of a round has its `finding.md` and `repro.sh` in the round's
evidence, outside the repository; a repro names the world it needs on its
second line (`# own-stack: ...`).
