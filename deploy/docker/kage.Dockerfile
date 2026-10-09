# Kage API image (spec §22): multi-stage build, BoringSSL build dependencies
# in the builder, distroless non-root runtime, dependency list embedded in the
# binary by cargo-auditable.
#
# Build context is the repository root:
#   docker build --file deploy/docker/kage.Dockerfile --tag kage:dev .

# The Rust tag is the toolchain pin for image builds; it must match
# rust-toolchain.toml (checked below). Bump both in the same PR.
ARG RUST_IMAGE=rust:1.99.0-trixie@sha256:6ff07edce8775d0f64be7aba9197229407301bddf2054d62c27b541a6238a181
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2

# --- builder ---------------------------------------------------------------
FROM ${RUST_IMAGE} AS builder

# wreq links BoringSSL, which its build script compiles from source. The
# packages come from the Debian release of the digest-pinned base image and
# exist in the builder stage only; the runtime image has no package manager.
# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install --yes --no-install-recommends cmake clang libclang-dev nasm perl golang \
    && rm -rf /var/lib/apt/lists/*

ARG CARGO_AUDITABLE_VERSION=0.7.7
RUN cargo install --locked "cargo-auditable@${CARGO_AUDITABLE_VERSION}"

# Fail early when the image toolchain drifts from rust-toolchain.toml. The
# file stays outside the build directory so rustup does not act on it.
COPY rust-toolchain.toml /tmp/rust-toolchain.toml
RUN expected="$(sed -n 's/^channel = "\(.*\)"$/\1/p' /tmp/rust-toolchain.toml)" \
    && actual="$(rustc --version)" \
    && case "${actual}" in \
        "rustc ${expected} "*) ;; \
        *) echo "rust-toolchain.toml pins ${expected} but this image has: ${actual}" >&2; exit 1 ;; \
    esac

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# The recorded fingerprints of the http path; kage-http's build script embeds
# them, so that `kage selftest http` has its expected values in the image.
COPY tests/fixtures/fingerprints ./tests/fixtures/fingerprints
# The sidecar contract; kage-browser's build script generates its types
# from this committed document.
COPY sidecar/packages/contract/openapi.json ./sidecar/packages/contract/openapi.json
# sqlx checks every query at compile time against this committed data, so the
# build needs no database.
COPY .sqlx ./.sqlx
ENV SQLX_OFFLINE=true

# Build outputs live in a cache mount, so the binary is copied out in the
# same step.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo auditable build --release --locked --package kage \
    && install -D --mode 0755 target/release/kage /out/usr/local/bin/kage \
    && install --directory --mode 0750 /out/var/lib/kage/artifacts

# --- runtime ---------------------------------------------------------------
FROM ${RUNTIME_IMAGE} AS runtime

LABEL org.opencontainers.image.title="kage" \
      org.opencontainers.image.description="Kage API server" \
      org.opencontainers.image.source="https://github.com/Serpe-nt/Kage" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"

# The binary stays root-owned and read-only for the runtime user.
COPY --from=builder /out/usr/local/bin/kage /usr/local/bin/kage
# State directory owned by the runtime user; a named volume mounted here
# inherits this ownership. Both sidecar images use the same numeric identity.
COPY --from=builder --chown=65532:65532 /out/var/lib/kage /var/lib/kage
# The policy files shipped with this version, read-only. Mount a directory
# over /etc/kage/policies to change them; kage reloads them on change.
COPY policies /etc/kage/policies

USER 65532:65532
ENV KAGE__SERVER__BIND=0.0.0.0:8080 \
    KAGE__POLICIES__DIR=/etc/kage/policies \
    KAGE__ARTIFACTS__DIR=/var/lib/kage/artifacts
EXPOSE 8080

# The image has no shell and no curl: the binary probes its own /healthz.
HEALTHCHECK --interval=10s --timeout=5s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/kage", "healthcheck"]

ENTRYPOINT ["/usr/local/bin/kage"]
CMD ["serve"]
