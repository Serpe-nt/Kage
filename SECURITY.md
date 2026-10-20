# Security policy

## Reporting a vulnerability

Report it privately through GitHub's advisory form: the **"Report a
vulnerability"** button on the
[Security tab](https://github.com/Serpe-nt/Kage/security) of the
repository (or
[this link](https://github.com/Serpe-nt/Kage/security/advisories/new)).
Please do not open a public issue or a pull request about it first.

A good report says what an attacker can reach or learn, from where (a
page Kage fetches, an API key with which scopes, a sidecar that was taken
over, the network next to the host), the version or commit, and the
smallest way to show it. Use `example.org`, a `*.test` name or the
fixture site of the end-to-end tests (`tests/fixtures/site`) for any site
in it; never a real proxy credential, an API key or anyone's personal
data.

Kage has one maintainer, who aims to answer within a week and to follow
with a fix or a written decision as soon as the work allows. A reporter is
credited in the advisory and the changelog unless they would rather not
be named. Please keep the report private until a fixed release is out or
a date is agreed.

## Supported versions

| Version | Security fixes |
|---|---|
| 2.0.x (the latest release line) | yes |
| 1.x and older | no |

Only the latest release line gets fixes. 2.0.0 breaks every 1.x request on
purpose; the upgrade is in
[docs/runbooks/levels-and-egress.md](docs/runbooks/levels-and-egress.md#upgrading-from-1x).
Releases are tags signed with the maintainer's key and images signed and
attested by the release workflow;
[docs/runbooks/repo.md](docs/runbooks/repo.md#releases) says how to verify
both before you run them.

## What is in scope

Anything that breaks a promise of the specification's security section
(`docs/01-spec.md` §19) or of the
[security review](docs/security-review.md), in particular:

- **The SSRF guard**, on every road an address enters: a fetch's URL, its
  redirects, a crawl's links and sitemaps, `robots.txt`, a flow's steps,
  webhooks, and the forwarder's own resolution under `egress.mode: direct`.
- **The forwarder**: a browser or client reaching anything but what the
  guard allows, a persona's credential opening another persona's or an
  exit's route, the provider's credentials reaching a sidecar.
- **Secrets**: the master key, the sidecar token, a provider's API key, a
  declared list's passwords, API keys, webhook secrets, and a caller's own
  `Cookie` or `Authorization` header at level 0, in a log line, a trace, an
  error, a stored result or an answer of the API.
- **The API's keys and scopes**: one key reading another's results,
  artifacts, jobs or persona handles; a key doing what its scopes do not
  allow; the admin endpoints without `admin`.
- **The sidecars' isolation**: what a browser that a hostile page took over
  can reach (the database, the host, the internet past the forwarder,
  files outside its own).
- **The egress rule**: a request leaving from an address its mode forbids
  (the host's address under `egress.mode: proxies`, the pool under
  `direct`, a site's level-0 traffic from more than its one exit).
- **The release supply chain**: a tag the release workflow accepts that it
  should refuse, an image that is not the one signed and attested.

## What is not

- **A site telling a persona apart from a person.** That is a tell, not a
  vulnerability: Kage exists to avoid them, and they are documented as
  limits where they are known (the
  [known tells](docs/security-review.md#known-tells), each engine's
  runbook). Open an issue with the bug form ("A site tells a persona
  apart"); report it privately first only when the issue would publish a
  technique that should not be public before a fix.
- **Vulnerabilities of Chrome, Camoufox, patchright, Playwright, wreq** or
  another dependency: report them to their projects. Kage pins exact
  versions (`docs/versions.md`) and moves a pin in a pull request of its
  own; tell us when a pinned version is affected.
- **An operator's own configuration**: names put in
  `egress.direct.private_names`, an API key given to someone, a port
  published beyond loopback without a TLS-terminating proxy, a policy that
  reads a site faster than a person.
- **The third-party detection pages** Kage's self-tests read, and the
  public echo and lookup services its probes use: their operators see what
  the runbooks say they see.
- Denial of service by a key that is allowed to send the requests in
  question, within the limits the operator set.
