# Runbook: the fingerprint echo and the http self-test

How Kage checks that its http path looks like the browser it claims to be,
how to run the echo that check needs, and what to do when a fingerprint
moves or a new browser major arrives. Spec §9 and §7.4 are the contract;
this page is the practice.

## What is compared

One typed navigation is sent to an echo, a server that answers with what the
request looked like on the wire. Five things are compared with the values
recorded for the profile:

| Check | What it is |
|---|---|
| `http_version` | The request arrived over HTTP/2. |
| `ja4` | The JA4 of the TLS ClientHello: versions, ciphers, extensions, ALPN, whether a server name was sent. |
| `akamai` | The HTTP/2 fingerprint: SETTINGS, the connection window update, priority frames, the order of the pseudo-headers. |
| `header_order` | The names of the request headers in the order they were sent. |
| `user_agent` | The `User-Agent` that arrived is the one the client meant to send. |

The recorded values are files, one per profile and claimed OS, in
`tests/fixtures/fingerprints/` (`chrome_149-windows.json` and so on). They
are compiled into the binary, so a deployed `kage` compares with the values
of its own build.

## Where the check runs

| Where | When | Echo | Network |
|---|---|---|---|
| `just ci` (`crates/kage-http/tests/fingerprints.rs`) | every change | the in-repo echo, in the test process | loopback only |
| `kage selftest http` | when an operator asks | the operator's own, else a third party | one request through one proxy |
| `tests/live/http-selftest.sh` (`just test-live`) | the gated live suite | the same choice | one request through each of two proxies |

The server never contacts a third-party echo on a timer. The scheduled
check of each persona (spec §7.4, every 24 h) arrives with personas in M6
and runs only against the operator's own echo, when `selftest.http.echo_url`
is set.

The CI gate is what makes a version bump safe: a change of the wreq or
wreq-util pin, or of Kage's header tables, that moves any of the five
values fails on that change, with no network involved.

## `kage selftest http`

```bash
kage selftest http --proxy <proxy id> --profile chrome_149
kage selftest http --proxy <proxy id> --profile firefox_151 --platform macos --json
```

| Flag | Meaning |
|---|---|
| `--proxy` | The proxy to leave through (`kage proxy list` shows the ids). A retired proxy is refused. |
| `--profile` | The emulation profile. `kage config check` lists the ones this build serves under `http.exact_profiles`. |
| `--platform` | The OS the profile claims: `windows` (default), `macos` or `linux`. |
| `--echo` | Ask this echo instead of the configured one. `https` only. |
| `--json` | One JSON document instead of a table. |

The echo that is asked is, in this order: `--echo`, `selftest.http.echo_url`,
`selftest.http.third_party_url` (default `https://tls.peet.ws/api/all`). The
first line of the output says which one and why.

**A third-party echo sees the exit address of the proxy that asks**, next to
a fingerprint that is Kage's. Ask it for a spot check, not for the pool. The
live suite asks it for two proxies per run and moves through the pool by
date for the same reason.

The command uses a fresh client: a new connection and no cookies, because a
resumed TLS session or a `Cookie` header changes what the echo sees. It
exits non-zero when a value differs, writes an audit event `selftest.http`
either way, and raises the alert `http_selftest_failed` (stored as a health
event for the proxy) on a difference. The last lines show where the time
went (tunnel, handshake, first byte, total) and the address the echo saw.

A Firefox profile with `--platform linux` is refused until M6: the profile's
own `User-Agent` names a Linux distribution, and only a persona supplies the
exact one.

### When it fails

| Differs | Likely cause | Do |
|---|---|---|
| `ja4`, with `i` where the recorded value has `d` (`t13i...`) | The echo was addressed by IP, so no server name was sent. | Address the echo by a DNS name. |
| `ja4`, `header_order` or `user_agent`, through one proxy only | Something on that proxy's path terminates TLS itself and sends its own request. | Treat the proxy as unsafe on both paths and replace it (`kage proxy replace`). |
| `ja4` or `akamai`, through every proxy | The wreq or wreq-util build is not the one the fixtures were recorded with. | Check `docs/versions.md` against `Cargo.lock`. A deliberate bump re-records the fixtures (below) in the same change. |
| `header_order`, through every proxy | Kage's header tables are not the ones the fixtures were recorded with. | Compare with the fixture; a deliberate change re-records it. |
| `http_version` | The echo, or something in front of it, offered HTTP/1.1 only. | Do not use an `h1.` host name for the self-test; check what fronts the echo. |

Until the difference is explained, do not send that profile to targets over
http. From M6 on a failed self-test also puts the persona into `cooling`
(spec §6.3).

"The self-test could not be judged" means there was no answer to compare:
the echo was unreachable, answered with an error, or is not an echo Kage
understands (its own format and the format of tls.peet.ws are understood).

## Running the echo

The echo is part of the `kage` binary. It needs no database and none of
Kage's secrets, only a certificate and its key:

```bash
kage echo serve --bind 0.0.0.0:8443 --cert cert.pem --key key.pem
```

`KAGE_ECHO_BIND`, `KAGE_ECHO_CERT` and `KAGE_ECHO_KEY` set the same three
values. In the compose stack it is the service `echo`, off unless its
profile is on:

```bash
docker compose --file deploy/compose.yaml --profile echo up --detach echo
```

`KAGE_ECHO_TLS_DIR` in `deploy/.env` names the directory with `cert.pem` and
`key.pem` (default `deploy/echo-tls`); `KAGE_ECHO_PUBLISH_ADDR` and
`KAGE_ECHO_PUBLISH_PORT` say where it is published. Then:

```yaml
selftest:
  http:
    echo_url: https://echo.example.org:8443/
```

What a deployment needs for the result to mean something:

- **A DNS name.** A browser sends the server name in its handshake, and JA4
  records whether one was sent. An echo addressed by IP gives every client
  a different JA4 from the recorded one.
- **A certificate the client trusts.** A public one (ACME) is the simple
  choice and the only one that also works for the browser captures of M4
  and M5 without touching the browsers' trust stores. For the http path
  alone a private authority works too: name its certificate in
  `selftest.http.echo_ca_file` and the self-test client trusts it in
  addition to the public roots. `kage config check` refuses a file it
  cannot read as PEM.
- **The key readable by the container's user**, uid 65532, and nobody else.
- **Reachable from the proxies' exits**, which means from the internet: the
  echo is the one service besides `kage` that publishes a port. It may run
  on the same host as Kage, because only the proxies' exits ever connect to
  it; the host's own address never does. It has no authentication and needs
  none: it tells a caller only what that caller sent. It accepts at most
  256 connections at once and reads at most 1 MB of request body.

The echo answers every path and method with one JSON document:

| Field | Content |
|---|---|
| `echo`, `version` | `kage-echo` and the format version, so its answers can be told from another service's. |
| `peer` | The address the connection came from: a proxy's exit. |
| `method`, `path`, `http_version` | The request. |
| `tls` | `ja4`, `ja4_r` (the lists written out), the offered version, server name, ALPN, ciphers, extensions, groups and signature algorithms. |
| `http2` | `akamai`, the SETTINGS, the window update, priority frames, the pseudo-header order, and every header of the HEADERS frame in wire order. |
| `http1` | For an HTTP/1.1 request instead: the request line and the header lines exactly as sent. |

A host name that starts with `h1.` is offered HTTP/1.1 only. That is how
the HTTP/1.1 header tables are exercised against a client that prefers
HTTP/2; give the name its own DNS record and include it in the certificate
when you want that outside the tests.

## Recording the fixtures

The fixtures change only on purpose: with a bump of wreq or wreq-util, with
a new profile, or with a change of the header tables that a browser capture
justifies.

```bash
KAGE_RECORD_FINGERPRINTS=1 cargo test -p kage-http --test fingerprints
```

writes one file per mapped profile and OS from what the in-repo echo
observed. Review the diff like any other change: every line that moved is a
change of what target sites see. Then run the tests again without the
variable; the build embeds the new files and the gate compares against
them.

The values recorded at M3 (wreq 0.16.1, wreq-util 0.2.0) equal the values
published for the real browsers: Chrome sends JA4
`t13d1516h2_8daaf6152771_d8a2da3f94cd`, Firefox
`t13d1717h2_5b57614c22b0_3cbfd9057e0d`.

## A new browser major

`kage config check` shows the state under `http`: the profiles served
exactly (`exact_profiles`, the newest eight majors per browser family that
wreq-util ships), the majors verified beyond them (`verified_majors`), and
the majors the engines bundle (`engines`).

`engines.chromium.browser_major` and `engines.firefox.browser_major` name
the browser each engine image bundles; M4 and M5 set them. A set major that
no profile serves fails `kage config check` and the profile test. That is
the gate working: a persona of that engine would otherwise present one
browser version in the browser and another over http.

To unlock a major that is newer than every wreq-util profile:

1. Point the real browser of that major at the echo, through a proxy, and
   keep the answer (the capture tooling arrives with M4 and M5).
2. Compare it with the fixture of the newest profile of that family: `ja4`,
   `akamai`, and the navigation header names and their order. All three
   must be equal. TLS and HTTP/2 alone are not enough: a newer
   `User-Agent` on an older header order is the same kind of mismatch.
3. When the headers differ, the header table in
   `crates/kage-http/src/headers.rs` gets an entry for that major first.
4. Add `(engine, major)` to `VERIFIED_MAJORS` in
   `crates/kage-http/src/profile.rs`, with the capture in the pull request.
   The http path then presents the newest profile's TLS and HTTP/2 with a
   `User-Agent` and `sec-ch-ua` written for the real major.

When the three values are not equal, the major stays unverified and
personas of that engine are browser-only until a wreq-util release carries
a matching profile. Bumping wreq-util is a fingerprint-critical change of
its own (`docs/versions.md`).
