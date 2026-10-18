# Runbook: repository, CI and releases

How the repository is configured, what CI runs, and how to cut a release.
Settings that live in GitHub rather than in git are written down here so they
can be re-applied.

## Branch protection on `main`

Spec §21 requires: pull requests only, required checks, linear history, and
conventional commits enforced by a check.

| Setting | Value |
|---|---|
| Require a pull request before merging | on |
| Required approvals | 0 while the project has one maintainer; raise it when a second one joins |
| Require status checks to pass | on, branch must be up to date |
| Required status check | `ci-ok` |
| Require linear history | on |
| Require conversation resolution | on |
| Allow force pushes, allow deletions | off |
| Apply to administrators | on |
| Allowed merge methods | squash and rebase; merge commits off (a milestone's pull request is merged with rebase, so that its commits stay as they were made) |
| Require signed commits | off for 1.0 (recommended below) |

`ci-ok` is the last job of `ci.yaml`. It fails unless the `commits`, `rust`,
`node`, `security` and `images` jobs all passed, so it is the only check that
has to be marked as required, and adding a job to CI does not need a settings
change.

Apply the protection with an admin token:

```bash
gh api --method PUT repos/Serpe-nt/Kage/branches/main/protection --input - <<'JSON'
{
  "required_status_checks": { "strict": true, "contexts": ["ci-ok"] },
  "enforce_admins": true,
  "required_pull_request_reviews": { "required_approving_review_count": 0 },
  "required_linear_history": true,
  "required_conversation_resolution": true,
  "allow_force_pushes": false,
  "allow_deletions": false,
  "restrictions": null
}
JSON
```

```bash
gh api --method PATCH repos/Serpe-nt/Kage \
  --field allow_merge_commit=false --field allow_squash_merge=true --field allow_rebase_merge=true \
  --field delete_branch_on_merge=true
```

Apply them once `ci-ok` has run on `main`: a required check must exist
before it can be required. The milestones M0 to M11 reached `main` without
pull requests (decision D30); the first push to `origin` fast-forwards
`main` by those commits, and `ci.yaml` runs on that push. From then on
every change goes through a pull request.

## Actions, the token and the scanners

Set once, with branch protection, by an admin of the repository:

| Setting | Value |
|---|---|
| Actions permissions | GitHub's own actions and the listed ones: `Swatinem/rust-cache@*`, `anchore/sbom-action@*`, `docker/build-push-action@*`, `docker/login-action@*`, `docker/metadata-action@*`, `docker/setup-buildx-action@*`, `pnpm/action-setup@*`, `sigstore/cosign-installer@*`, `taiki-e/install-action@*` |
| Require actions to be pinned to a full-length commit SHA | on (every action is pinned so already) |
| Default `GITHUB_TOKEN` permissions | read only; a workflow asks for more per job |
| Allow GitHub Actions to create and approve pull requests | off |
| Fork pull requests | approval required for every outside contributor before a run |
| Secret scanning, push protection | on |
| Dependabot alerts | on; version updates off (pins move in a pull request of their own, the fingerprint-critical ones never by a bot, `docs/versions.md`) |

```bash
gh api --method PUT repos/Serpe-nt/Kage/actions/permissions \
  --field enabled=true --field allowed_actions=selected --field sha_pinning_required=true
gh api --method PUT repos/Serpe-nt/Kage/actions/permissions/selected-actions --input - <<'JSON'
{
  "github_owned_allowed": true,
  "verified_allowed": false,
  "patterns_allowed": [
    "Swatinem/rust-cache@*", "anchore/sbom-action@*", "docker/build-push-action@*",
    "docker/login-action@*", "docker/metadata-action@*", "docker/setup-buildx-action@*",
    "pnpm/action-setup@*", "sigstore/cosign-installer@*", "taiki-e/install-action@*"
  ]
}
JSON
gh api --method PUT repos/Serpe-nt/Kage/actions/permissions/workflow \
  --field default_workflow_permissions=read --field can_approve_pull_request_reviews=false
gh api --method PUT repos/Serpe-nt/Kage/actions/permissions/fork-pr-contributor-approval \
  --field approval_policy=all_external_contributors
gh api --method PATCH repos/Serpe-nt/Kage --input - <<'JSON'
{ "security_and_analysis": {
    "secret_scanning": { "status": "enabled" },
    "secret_scanning_push_protection": { "status": "enabled" } } }
JSON
gh api --method PUT repos/Serpe-nt/Kage/vulnerability-alerts
```

A new action in a workflow needs its owner in the list above, here and in
the setting, or its run fails before the first step.

## Commits

Commit subjects and pull request titles follow
[Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/):
`<type>(<scope>)?!?: <description>` with the types `build`, `chore`, `ci`,
`docs`, `feat`, `fix`, `perf`, `refactor`, `revert`, `style`, `test`.

- Locally, the `commit-msg` hook rejects other subjects. `just setup`
  installs the hooks by pointing `core.hooksPath` at `.githooks/`.
- In CI, the `commits` job checks the pull request title and every commit
  the pull request adds.

Every commit records its author's name and e-mail address, and a public
repository shows them. To keep an address out of new commits, turn on
"Keep my email addresses private" in GitHub's e-mail settings (and "Block
command line pushes that expose my email") and commit with the address
GitHub gives there:

```bash
git config user.email "<id>+<user>@users.noreply.github.com"
```

The commits made before stay as they are. Signed commits are not required;
to sign them anyway with the same SSH key as the release tags:

```bash
git config gpg.format ssh
git config user.signingkey ~/.ssh/kage_release_signing_ed25519.pub
git config commit.gpgsign true
```

## What CI runs

`ci.yaml` runs on every pull request, on pushes to `main` and in the merge
queue. Each job calls the same `just` targets a developer runs locally, and
`just ci` runs all of them in one go.

| Job | `just` targets | Gates |
|---|---|---|
| `commits` | `scripts/check-conventional-commits.sh` | PR title and commit subjects |
| `rust` | `fmt-check-rust`, `lint-rust`, `setup-sqlx`, `sqlx-check`, `test-rust`, `cov-rust` | rustfmt, clippy `-D warnings`, migrations and committed query data, tests, 80 % line coverage on the gated crates |
| `node` | `fmt-check-node`, `lint-node`, `lint-observability`, `contract-check`, `test-node`, `cov-node` | prettier, tsc, eslint, the dashboards and alert rules (promtool), the sidecar contract, vitest, 80 % line coverage on `engine-core` |
| `security` | `deny`, `hardening`, `secrets`, `sbom` | cargo-deny, cargo-audit, pnpm audit, sidecar license policy, `openssl-sys` check, the container checklist on the Dockerfiles and compose files, gitleaks, CycloneDX SBOMs |
| `images` | `build-images`, `hardening-images`, `scan-images`, `sbom-images`, `test-e2e` | image builds, the container checklist on the images, Trivy (fixable HIGH and CRITICAL), SBOM per image, the stack end to end and the load test's design A |
| `mutants` | `mutants` | reported on every pull request; the gate (70 % on `kage-core`) is run before a milestone is merged |
| `ci-ok` | none | fails unless all required jobs passed |

Coverage gates:

- Rust: `scripts/check-rust-coverage.mjs` reads the llvm-cov summary and
  fails when `kage-core`, `kage-store`, `kage-proxy`, `kage-crawl` or
  `kage-extract` is below 80 % line coverage. The list and the threshold are
  `cov_gated_crates` and `cov_min_lines` in the `justfile`.
- Sidecar: the threshold for `engine-core` is in `sidecar/vitest.config.ts`.

Database gates (`just sqlx-check`, `scripts/check-migrations.sh`): against a
throwaway Postgres in Docker, the migrations apply from an empty database,
`sqlx migrate info` shows every one installed with the checksum of its file,
applying them again changes nothing, and `.sqlx/` matches the queries in the
code. After changing a query or a migration, run `just sqlx-prepare` and
commit `.sqlx/`. Builds never need a database: the `justfile` and the image
build set `SQLX_OFFLINE=true`.

The `rust` job needs Docker: the store and CLI tests start Postgres
containers through testcontainers.

## Where versions are pinned

`docs/versions.md` is the record. The pins themselves live in:

| What | Where |
|---|---|
| Rust toolchain | `rust-toolchain.toml`, and the builder tag in `deploy/docker/kage.Dockerfile` (the image build fails when they differ) |
| Rust crates | `[workspace.dependencies]` in `Cargo.toml`, `Cargo.lock` |
| Node | `sidecar/.nvmrc`, and the base tag in `deploy/docker/sidecar.Dockerfile` |
| pnpm | `packageManager` in `sidecar/package.json` |
| npm packages | each `package.json`, `sidecar/pnpm-lock.yaml` |
| cargo tools, scanner images | variables at the top of the `justfile` |
| just (CI bootstrap) | `.github/actions/setup-just/action.yml` |
| GitHub Actions | commit SHAs in `.github/` |
| Base images, Postgres | digests in `deploy/docker/` and `deploy/compose.yaml` |

A bump of anything in the fingerprint-critical table of `docs/versions.md`
is its own pull request and is never auto-merged.

## Nightly live suite

`nightly-live.yaml` runs the live suite (`just test-live`). It needs real
proxies and only ever hits the echo and detection test pages listed in
configuration, never a real target site.

Beside it, every night and without a secret, the `load` job runs the load
test's design B (`just test-load`): 200 queued fetches of the fixture site
under the test-only `load` profile at the shipped caps, in 540 s to 720 s,
in the fixture world of the end-to-end tests. It reaches no proxy and no
site of anyone's. Run it by hand, once, on the commit a release is tagged
on.

- The nightly schedule is skipped until the repository variable
  `KAGE_LIVE_ENABLED` is set to `true`.
- A manual run (`workflow_dispatch`) always executes and fails with a clear
  error when a secret is missing.
- Repository secrets it reads: `KAGE_WEBSHARE_API_KEY` and
  `KAGE_SECRETS_MASTER_KEY`.
- The repository variable `KAGE_LIVE_EXPECT_PROXIES` says how many proxies
  the Webshare plan has, when that is not 20.

The suite is `tests/live/run.sh`, one script per check:

| Check | Since | What it does |
|---|---|---|
| `tests/live/proxy-health.sh` | M2 | Reads the real proxy list into a throwaway Postgres and probes every proxy once with `kage proxy health`. Passes when the list has the expected number of proxies and each answers with a latency and a country. Contacts the Webshare API and the endpoints of `proxies.health` only ([proxies.md](proxies.md)). Its output names no proxy address. |
| `tests/live/http-selftest.sh` | M3 | Reads the real proxy list into a throwaway Postgres, picks two proxies by the date, and runs `kage selftest http` through each, one as the newest Chrome profile and one as the newest Firefox profile. Passes when both fingerprints equal the recorded ones. Contacts the Webshare API and one fingerprint echo: the operator's own when `KAGE__SELFTEST__HTTP__ECHO_URL` is set, else the third-party echo of `selftest.http.third_party_url`, which sees the two exit addresses ([fingerprint-echo.md](fingerprint-echo.md)). Requests no replacement. |
| `tests/live/browser-selftest.sh` | M4, M5 | For each engine of `KAGE_LIVE_ENGINES` (default `chromium firefox`), one after the other: starts that engine's sidecar image, hardened as in production, and a throwaway Postgres; picks two proxies by the date, never the two of the http check and the same two for every engine; runs `kage selftest browser --engine <engine> --third-party` through each. Passes when no check carries an automation flag or an identity mismatch, both pages could be read, the score is above the threshold, and no check is worse than in the baseline of that engine and major (`tests/fixtures/selftest/<engine>-<major>-live.json`); an engine without a baseline fails until one is recorded. Contacts the Webshare API and the pages of `selftest.browser.third_party_pages` (bot.sannysoft.com and CreepJS), which see the two exit addresses ([engine-chromium.md](engine-chromium.md), [engine-firefox.md](engine-firefox.md)). Requests no replacement. `KAGE_LIVE_RECORD=1` records a passing run as the baseline; the first recording of a new major is judged against the newest baseline of an older major of the same engine, and the old file is removed with the old image. `KAGE_LIVE_FIREFOX_CLAIMS` names the systems the two Firefox stand-ins claim (default `windows windows`; `windows macos` checks both). `KAGE_LIVE_IDLE_SECONDS` adds a measurement of the browser's own background traffic. The repository variable `KAGE_LIVE_ENGINES` narrows the nightly run to one engine. |
| `tests/live/personas.sh` | M6 | Only with `KAGE_LIVE_PERSONAS=1`, never in the nightly run. Starts both sidecar images, a throwaway Postgres and `kage serve` with warm-up off; waits until every usable proxy of the real pool has its persona and checks them (`tests/live/check-personas.mjs`: one persona per proxy, each where its proxy was observed to be, the mix within two personas of its shares, no shared seed, at least three screens); then runs `kage persona selftest --third-party` for one Chrome and one Firefox persona through their own proxies and judges the reports with the same baselines as the browser check. Contacts the Webshare API, the provider's echo and the geo lookup through every proxy (as the server does), and the pages of `selftest.browser.third_party_pages` through two proxies, which see those two exit addresses ([personas.md](personas.md)). Requests no replacement; no persona visits a site by itself. |
| `tests/live/jobs.sh` | M9 | Only with `KAGE_LIVE_JOBS=1`, never in the nightly run. Starts both sidecar images, a throwaway Postgres, a receiver of webhooks on the loopback and `kage serve` with warm-up off and the replacement budget at 0, at the shipped `human` pace, and asks the API for the two third-party detection pages only: a synchronous fetch in each engine, a batch of four items, a flow in each engine, their webhooks, and a fetch carried across a restart of the server (about a dozen page loads through the proxies of a few personas). `KAGE_LIVE_JOBS_ONLY=flows` runs the two flows alone (four page loads). A job never wakes a persona: at night in the pool's countries set `KAGE_LIVE_JOBS_ACTIVE_HOURS=ignore`. `tests/live/check-jobs.mjs` judges it and writes evidence without addresses. |

M12 wires the workflow completely.

To run it locally:

```bash
KAGE_LIVE=1 KAGE__WEBSHARE__API_KEY=... KAGE__SECRETS__MASTER_KEY=... just test-live
```

## Releases

1. On `main`, set the version in `[workspace.package]` of `Cargo.toml`, move
   the Unreleased entries of `CHANGELOG.md` under the new version, and merge
   that pull request.
2. Wait for `ci` on the merge commit, and run the `load` job of
   `nightly-live.yaml` on it once by hand.
3. Tag the merge commit with an annotated tag signed by the owner's SSH key,
   and push the tag:

   ```bash
   git config gpg.format ssh
   git config user.signingkey ~/.ssh/kage_release_signing_ed25519.pub
   git tag --sign --message "Kage X.Y.Z" vX.Y.Z <merge commit>
   git push origin vX.Y.Z
   ```

   The public half of the key is listed in `.github/allowed_signers` on
   `main`, one line per key, with the tagger's e-mail address:
   `<e-mail> namespaces="git" ssh-ed25519 AAAA...`. Check a tag before it is
   pushed with `git -c gpg.ssh.allowedSignersFile=.github/allowed_signers
   verify-tag vX.Y.Z`.
4. `release.yaml` refuses a tag that is lightweight, unsigned, signed by a
   key the list on `main` does not hold, not on `main`, or not the
   workspace version. Then, for `ghcr.io/serpe-nt/kage` and
   `ghcr.io/serpe-nt/kage-browser-chromium`, it builds and pushes the image,
   scans it with Trivy, signs it by digest with cosign (keyless: the
   signature is made under the workflow's own identity, from GitHub's OIDC
   token, and is recorded in Sigstore's transparency log), and attests its
   SLSA build provenance and its CycloneDX SBOM to the digest. The GitHub
   release has each digest and each SBOM.
5. Open a follow-up pull request that records the digests in
   `deploy/compose.yaml`.

Only the `images` job of `release.yaml` holds `id-token: write` and
`attestations: write`; no job holds a signing key, and no repository secret
is needed to release.

Verify an image before it is run, for the tag `vX.Y.Z` and the digest of
its release:

```bash
cosign verify ghcr.io/serpe-nt/kage@sha256:<digest> \
  --certificate-identity "https://github.com/Serpe-nt/Kage/.github/workflows/release.yaml@refs/tags/vX.Y.Z" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
gh attestation verify oci://ghcr.io/serpe-nt/kage@sha256:<digest> \
  --repo Serpe-nt/Kage --signer-workflow Serpe-nt/Kage/.github/workflows/release.yaml
gh attestation verify oci://ghcr.io/serpe-nt/kage@sha256:<digest> \
  --repo Serpe-nt/Kage --predicate-type https://cyclonedx.org/bom
```

The same three for `ghcr.io/serpe-nt/kage-browser-chromium`.

The Firefox sidecar image is not published (ADR 0008, outcome of M5): the
Camoufox build it unpacks bundles Microsoft's and Apple's font files, and a
public image would redistribute them. An operator builds it where it runs
(`just build-images`); CI builds and scans it on every pull request.
Repository secrets: `KAGE_WEBSHARE_API_KEY` and `KAGE_SECRETS_MASTER_KEY`
for the live suite only. Repository variables: `KAGE_LIVE_ENABLED`,
`KAGE_LIVE_EXPECT_PROXIES`, `KAGE_LIVE_ENGINES`.
