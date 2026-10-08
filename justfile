# Kage task runner: every dev and CI command is a target here (spec §4).
#
# Recipes need bash and Docker, so run them on Linux, in WSL2 or in a
# devcontainer. Install `just` itself first, then run `just setup`.

set shell := ["bash", "-euo", "pipefail", "-c"]

# --- Pinned tools (docs/versions.md) -----------------------------------------

cargo_deny_version := "0.20.2"
cargo_audit_version := "0.22.2"
cargo_llvm_cov_version := "0.9.1"
cargo_cyclonedx_version := "0.5.9"
cargo_mutants_version := "27.1.0"

# Installed by `setup-sqlx`, not through `rust_tools`: it is built with rustls
# and Postgres only, which no prebuilt binary offers.
sqlx_cli_version := "0.9.0"

# `tool@version,...` for taiki-e/install-action in CI: `just --evaluate rust_tools`.
rust_tools := "cargo-deny@" + cargo_deny_version + ",cargo-audit@" + cargo_audit_version + ",cargo-llvm-cov@" + cargo_llvm_cov_version + ",cargo-cyclonedx@" + cargo_cyclonedx_version + ",cargo-mutants@" + cargo_mutants_version

# Scanners run from images pinned by digest, so no host install is needed.
gitleaks_image := "ghcr.io/gitleaks/gitleaks:v8.30.1@sha256:c00b6bd0aeb3071cbcb79009cb16a60dd9e0a7c60e2be9ab65d25e6bc8abbb7f"
trivy_image := "ghcr.io/aquasecurity/trivy:0.75.0@sha256:af6acf9a6b85dfe389a1941505c0ce9efef52a4719635e1a962f022a3d855daa"
syft_image := "ghcr.io/anchore/syft:v1.54.1@sha256:3eb5379ba7b409c3f4069b686110527af0c47df993fa5c10d13e7cf34f49b1aa"

# --- Project settings --------------------------------------------------------

compose := "docker compose --file deploy/compose.yaml"

# Image tags built by `build-images`; they match the defaults in deploy/compose.yaml.
images := "kage:dev kage-browser-chromium:dev kage-browser-firefox:dev"

# Spec §21: these crates must keep at least this line coverage. kage-store
# joined the list in M1 (docs/02-plan.md, M1 acceptance). The sidecar gate
# (engine-core) lives in sidecar/vitest.config.ts.
cov_gated_crates := "kage-core,kage-store,kage-proxy,kage-crawl,kage-extract"
cov_min_lines := "80"

# sqlx checks every query at compile time against the committed data in
# .sqlx/, never against a live database. `sqlx-prepare` regenerates it.
export SQLX_OFFLINE := "true"

# Base of the commit range checked by `commits`.
base_ref := env("KAGE_BASE_REF", "origin/main")

# List the available targets.
default:
    @just --list --unsorted

# --- Setup -------------------------------------------------------------------

# Install toolchains, pnpm deps, git hooks and local dev secrets.
setup: setup-rust setup-sqlx setup-node setup-hooks setup-env

# Install the pinned Rust toolchain and cargo tools.
setup-rust:
    #!/usr/bin/env bash
    set -euo pipefail
    rustup toolchain install
    IFS=',' read -ra tools <<< "{{ rust_tools }}"
    for tool in "${tools[@]}"; do
        name="${tool%@*}"
        version="${tool#*@}"
        if cargo install --list | grep -q "^${name} v${version}:"; then
            echo "${name} ${version} is already installed"
        elif cargo binstall --version > /dev/null 2>&1; then
            cargo binstall --no-confirm "${name}@${version}"
        else
            cargo install --locked "${name}@${version}"
        fi
    done

# Install the pinned sqlx-cli, built with rustls and Postgres support only.
setup-sqlx:
    #!/usr/bin/env bash
    set -euo pipefail
    if cargo install --list | grep -q "^sqlx-cli v{{ sqlx_cli_version }}:"; then
        echo "sqlx-cli {{ sqlx_cli_version }} is already installed"
    else
        cargo install --locked "sqlx-cli@{{ sqlx_cli_version }}" \
            --no-default-features --features rustls,postgres
    fi

# Install the pinned pnpm and the sidecar dependencies.
setup-node:
    command -v pnpm > /dev/null || corepack enable pnpm
    cd sidecar && pnpm install --frozen-lockfile

# Point git at the hooks in .githooks/.
setup-hooks:
    git config core.hooksPath .githooks

# Create deploy/.env with random local secrets when it is missing.
setup-env:
    scripts/dev-secrets.sh

# --- Format ------------------------------------------------------------------

# Format everything: rustfmt + prettier.
fmt:
    cargo fmt --all
    cd sidecar && pnpm run format

# Check formatting without changing files.
fmt-check: fmt-check-rust fmt-check-node

fmt-check-rust:
    cargo fmt --all --check

fmt-check-node:
    cd sidecar && pnpm run format:check

# --- Lint --------------------------------------------------------------------

# clippy -D warnings, eslint, tsc.
lint: lint-rust lint-node

lint-rust:
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

# Builds first: packages type-check against each other's emitted declarations.
lint-node:
    cd sidecar && pnpm run build && pnpm run typecheck && pnpm run lint

# --- Test --------------------------------------------------------------------

# Rust unit + integration tests, sidecar vitest.
test: test-rust test-node

# The store and CLI tests start Postgres in Docker (testcontainers).
test-rust:
    cargo test --workspace --all-features --locked

# Migrations apply from empty, are idempotent, `sqlx migrate info` is clean,
# and .sqlx/ matches the queries. Uses a throwaway Postgres in Docker.
sqlx-check:
    scripts/check-migrations.sh

# Regenerate .sqlx/ after a query or a migration changed.
sqlx-prepare:
    scripts/check-migrations.sh --prepare

test-node:
    cd sidecar && pnpm run build && pnpm run test

# Compose-based end-to-end suite. At M0: the stack smoke test.
test-e2e: setup-env
    tests/e2e/smoke.sh

# Gated live suite: needs KAGE_LIVE=1 and secrets, never runs in PR CI.
test-live:
    tests/live/run.sh

# Mutation testing on kage-core. Reported only; gating starts in M7.
mutants:
    cargo mutants --package kage-core --output target/mutants

# --- Coverage ----------------------------------------------------------------

# Coverage reports with thresholds.
cov: cov-rust cov-node

# Runs the Rust tests instrumented, then enforces the per-crate threshold.
cov-rust:
    mkdir -p target/coverage
    cargo llvm-cov --workspace --all-features --locked --no-report
    cargo llvm-cov report --lcov --output-path target/coverage/rust.lcov
    cargo llvm-cov report --json --summary-only --output-path target/coverage/rust.json
    node scripts/check-rust-coverage.mjs --min-lines {{ cov_min_lines }} --gate {{ cov_gated_crates }} target/coverage/rust.json

cov-node:
    cd sidecar && pnpm run build && pnpm run test:coverage

# --- Supply chain ------------------------------------------------------------

# cargo-deny, cargo-audit, pnpm audit, sidecar license policy, openssl-sys check.
deny: audit
    cargo deny --all-features --locked check
    scripts/check-no-openssl.sh
    node scripts/check-npm-licenses.mjs

# cargo-audit and pnpm audit.
audit:
    cargo audit
    cd sidecar && pnpm audit --audit-level high

# Secret scan with gitleaks: git history when available, else the working tree.
secrets:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -d .git ]; then
        docker run --rm --volume "$PWD:/repo:ro" --workdir /repo "{{ gitleaks_image }}" \
            git --redact --no-banner --exit-code 1 .
    else
        echo "No .git directory here (linked worktree or export): scanning files instead of history."
        docker run --rm --volume "$PWD:/repo:ro" --workdir /repo "{{ gitleaks_image }}" \
            dir --redact --no-banner --exit-code 1 .
    fi

# CycloneDX SBOMs for the Rust workspace and the sidecar, in target/sbom/.
sbom:
    #!/usr/bin/env bash
    set -euo pipefail
    out="target/sbom"
    rm -rf "$out" && mkdir -p "$out"
    cargo cyclonedx --format json --all-features
    # cargo-cyclonedx writes next to each manifest; collect the files in one place.
    find crates -maxdepth 2 -name '*.cdx.json' -exec mv {} "$out/" \;
    docker run --rm --volume "$PWD:/repo:ro" --volume "$PWD/$out:/out" "{{ syft_image }}" \
        scan dir:/repo/sidecar --source-name kage-sidecar --output cyclonedx-json=/out/sidecar.cdx.json
    ls -1 "$out"

# --- Images ------------------------------------------------------------------

# Docker images for kage and both sidecar variants.
build-images: setup-env
    {{ compose }} build

# Trivy scan of every image: fails on fixable HIGH or CRITICAL findings.
scan-images:
    #!/usr/bin/env bash
    set -euo pipefail
    for image in {{ images }}; do
        echo "==> trivy ${image}"
        docker run --rm \
            --volume /var/run/docker.sock:/var/run/docker.sock \
            --volume kage-trivy-cache:/root/.cache/trivy \
            --volume "$PWD/.trivyignore:/.trivyignore:ro" \
            "{{ trivy_image }}" image \
            --scanners vuln --severity HIGH,CRITICAL --ignore-unfixed \
            --ignorefile /.trivyignore --exit-code 1 --no-progress "${image}"
    done

# CycloneDX SBOM per image, in target/sbom/.
sbom-images:
    #!/usr/bin/env bash
    set -euo pipefail
    out="target/sbom"
    mkdir -p "$out"
    for image in {{ images }}; do
        name="${image%%:*}"
        docker run --rm \
            --volume /var/run/docker.sock:/var/run/docker.sock \
            --volume "$PWD/$out:/out" \
            "{{ syft_image }}" scan "docker:${image}" --output "cyclonedx-json=/out/image-${name}.cdx.json"
    done
    ls -1 "$out"

# --- Run ---------------------------------------------------------------------

# Postgres + sidecars in compose, kage serve natively.
dev: setup-env
    {{ compose }} --file deploy/compose.dev.yaml up --detach --build --wait postgres browser-chromium browser-firefox
    source scripts/dev-env.sh && cargo run --package kage -- migrate
    source scripts/dev-env.sh && KAGE__SERVER__BIND=127.0.0.1:8080 cargo run --package kage -- serve

# Apply migrations to the development database (the Postgres of `just dev`).
migrate: setup-env
    source scripts/dev-env.sh && cargo run --package kage -- migrate

# Validate the development configuration and print it with secrets redacted.
config-check: setup-env
    source scripts/dev-env.sh && cargo run --quiet --package kage -- config check

# Regenerate the public and sidecar OpenAPI files.
openapi:
    @echo "No OpenAPI documents are committed yet: the sidecar contract lands in M4 and the public API in M8 (docs/02-plan.md). Nothing to regenerate."

# --- CI ----------------------------------------------------------------------

# Conventional-commit check on the commits a PR would add.
commits:
    #!/usr/bin/env bash
    set -euo pipefail
    if git rev-parse --verify --quiet "{{ base_ref }}^{commit}" > /dev/null; then
        scripts/check-conventional-commits.sh --range "{{ base_ref }}..HEAD"
    else
        echo "Base ref {{ base_ref }} not found: skipping the commit check (set KAGE_BASE_REF)."
    fi

# Everything a PR runs.
ci: commits fmt-check lint sqlx-check test cov deny secrets sbom build-images scan-images sbom-images test-e2e
    @echo "ci: all gates passed"
