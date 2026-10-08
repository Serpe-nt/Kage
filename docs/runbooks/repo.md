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
| Allowed merge methods | squash and rebase; merge commits off |

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

M0 did not apply these settings: it documents them. Apply them once the M0
pull request is merged, because `ci-ok` only exists as a check after
`ci.yaml` is on `main`.

## Commits

Commit subjects and pull request titles follow
[Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/):
`<type>(<scope>)?!?: <description>` with the types `build`, `chore`, `ci`,
`docs`, `feat`, `fix`, `perf`, `refactor`, `revert`, `style`, `test`.

- Locally, the `commit-msg` hook rejects other subjects. `just setup`
  installs the hooks by pointing `core.hooksPath` at `.githooks/`.
- In CI, the `commits` job checks the pull request title and every commit
  the pull request adds.

## What CI runs

`ci.yaml` runs on every pull request, on pushes to `main` and in the merge
queue. Each job calls the same `just` targets a developer runs locally, and
`just ci` runs all of them in one go.

| Job | `just` targets | Gates |
|---|---|---|
| `commits` | `scripts/check-conventional-commits.sh` | PR title and commit subjects |
| `rust` | `fmt-check-rust`, `lint-rust`, `test-rust`, `cov-rust` | rustfmt, clippy `-D warnings`, tests, 80 % line coverage on the gated crates |
| `node` | `fmt-check-node`, `lint-node`, `test-node`, `cov-node` | prettier, tsc, eslint, vitest, 80 % line coverage on `engine-core` |
| `security` | `deny`, `secrets`, `sbom` | cargo-deny, cargo-audit, pnpm audit, sidecar license policy, `openssl-sys` check, gitleaks, CycloneDX SBOMs |
| `images` | `build-images`, `scan-images`, `sbom-images`, `test-e2e` | image builds, Trivy (fixable HIGH and CRITICAL), SBOM per image, stack smoke test |
| `mutants` | `mutants` | report only until M7 |
| `ci-ok` | none | fails unless all required jobs passed |

Coverage gates:

- Rust: `scripts/check-rust-coverage.mjs` reads the llvm-cov summary and
  fails when `kage-core`, `kage-proxy`, `kage-crawl` or `kage-extract` is
  below 80 % line coverage. The list and the threshold are `cov_gated_crates`
  and `cov_min_lines` in the `justfile`.
- Sidecar: the threshold for `engine-core` is in `sidecar/vitest.config.ts`.

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

`nightly-live.yaml` runs the live fingerprint regression suite. It needs real
proxies and only ever hits the echo and detection test pages listed in
configuration, never a real target site.

- The nightly schedule is skipped until the repository variable
  `KAGE_LIVE_ENABLED` is set to `true`.
- A manual run (`workflow_dispatch`) always executes and fails with a clear
  error when a secret is missing.
- Repository secrets it reads: `KAGE_WEBSHARE_API_KEY` and
  `KAGE_SECRETS_MASTER_KEY`.

The workflow is a skeleton in M0: `just test-live` fails on purpose because
no live test exists yet. The first tests land in M2 and M3; M12 wires the
workflow completely.

## Releases

1. On `main`, set the version in `[workspace.package]` of `Cargo.toml`, move
   the Unreleased entries of `CHANGELOG.md` under the new version, and merge
   that pull request.
2. Tag the merge commit `vX.Y.Z` and push the tag.
3. `release.yaml` checks that the tag is on `main` and matches the workspace
   version, builds and pushes `ghcr.io/serpe-nt/kage`,
   `ghcr.io/serpe-nt/kage-browser-chromium` and
   `ghcr.io/serpe-nt/kage-browser-firefox`, and creates the GitHub release
   with each image digest and a CycloneDX SBOM per image.
4. Open a follow-up pull request that records the digests in
   `deploy/compose.yaml`.

Before the first release: ADR 0008 must be decided, because it determines
whether the Firefox image may be published. Signed tags and signed images are
part of M12.
