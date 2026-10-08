# Browser sidecar image (spec §22): one image per engine from the same code.
#
# Build context is the repository root; ENGINE selects the variant:
#   docker build --file deploy/docker/sidecar.Dockerfile --build-arg ENGINE=chromium --tag kage-browser-chromium:dev .
#   docker build --file deploy/docker/sidecar.Dockerfile --build-arg ENGINE=firefox  --tag kage-browser-firefox:dev .
#
# Each variant ships the production dependencies of its own engine only, so
# the two playwright versions never meet in one image (spec risk R2).

# Must match sidecar/.nvmrc. Bump both in the same PR.
ARG NODE_IMAGE=node:24.21.0-trixie-slim@sha256:173f125896c3b47ddf056734c7ea789d04595a6a08769a8f78e0df642781fb66

# --- base: Node with the pinned pnpm ---------------------------------------
FROM ${NODE_IMAGE} AS base
ENV COREPACK_ENABLE_DOWNLOAD_PROMPT=0 \
    CI=true
WORKDIR /app/sidecar
# corepack takes the exact pnpm version and its hash from "packageManager".
COPY sidecar/package.json ./
RUN corepack enable pnpm && pnpm --version

# --- build: every dependency, compile every package -------------------------
FROM base AS build
COPY sidecar/pnpm-lock.yaml sidecar/pnpm-workspace.yaml sidecar/tsconfig.base.json sidecar/tsconfig.json ./
COPY sidecar/packages ./packages
RUN --mount=type=cache,id=kage-pnpm-store,target=/pnpm/store \
    pnpm install --frozen-lockfile --store-dir /pnpm/store
RUN pnpm run build

# --- deps: production dependencies of the server and ONE engine -------------
FROM base AS deps
ARG ENGINE
COPY sidecar/pnpm-lock.yaml sidecar/pnpm-workspace.yaml ./
COPY sidecar/packages/contract/package.json packages/contract/
COPY sidecar/packages/engine-core/package.json packages/engine-core/
COPY sidecar/packages/engine-chromium/package.json packages/engine-chromium/
COPY sidecar/packages/engine-firefox/package.json packages/engine-firefox/
COPY sidecar/packages/server/package.json packages/server/
RUN --mount=type=cache,id=kage-pnpm-store,target=/pnpm/store \
    case "${ENGINE}" in \
        chromium) other=firefox ;; \
        firefox) other=chromium ;; \
        *) echo "build arg ENGINE must be chromium or firefox, got: ${ENGINE:-<unset>}" >&2; exit 1 ;; \
    esac \
    && pnpm install --frozen-lockfile --prod --store-dir /pnpm/store \
        --filter "@kage/server..." --filter "!@kage/engine-${other}" \
    && rm -rf "packages/engine-${other}"
COPY --from=build /app/sidecar/packages/contract/dist packages/contract/dist
COPY --from=build /app/sidecar/packages/engine-core/dist packages/engine-core/dist
COPY --from=build /app/sidecar/packages/engine-${ENGINE}/dist packages/engine-${ENGINE}/dist
COPY --from=build /app/sidecar/packages/server/dist packages/server/dist

# --- runtime ---------------------------------------------------------------
FROM ${NODE_IMAGE} AS runtime
ARG ENGINE

LABEL org.opencontainers.image.title="kage-browser-${ENGINE}" \
      org.opencontainers.image.description="Kage browser sidecar, ${ENGINE} engine" \
      org.opencontainers.image.source="https://github.com/Serpe-nt/Kage" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"

# Engine-specific system packages and the browser itself are installed here.
# Stubbed in M0 (docs/02-plan.md):
#   chromium  Chrome stable and Xvfb land in M4.
#   firefox   the pinned Camoufox build is fetched in M5.
# Both variants record what they run under /etc/kage/.

# The runtime needs node only: drop the package managers. The user has the
# same numeric identity as in the kage image, so both can share the artifacts
# volume.
RUN rm -rf /usr/local/lib/node_modules /usr/local/bin/npm /usr/local/bin/npx \
        /usr/local/bin/corepack /usr/local/bin/yarn /usr/local/bin/yarnpkg /opt/yarn-* \
    && groupadd --gid 65532 kage \
    && useradd --uid 65532 --gid 65532 --no-create-home --home-dir /nonexistent \
        --shell /usr/sbin/nologin kage \
    && install --directory --owner kage --group kage --mode 0750 \
        /var/lib/kage/personas /var/lib/kage/artifacts \
    && install --directory /etc/kage \
    && printf '%s\n' "${ENGINE}" > /etc/kage/engine \
    && node --version > /etc/kage/node-version \
    && printf '%s\n' "none (engine stub: the browser is installed from M4 and M5 on)" \
        > /etc/kage/browser-version

COPY --from=deps /app/sidecar /app/sidecar

ENV NODE_ENV=production \
    ENGINE=${ENGINE} \
    SIDECAR_HOST=0.0.0.0 \
    SIDECAR_PORT=8090
USER 65532:65532
WORKDIR /app/sidecar/packages/server
EXPOSE 8090

HEALTHCHECK --interval=10s --timeout=5s --start-period=10s --retries=3 \
    CMD ["node", "dist/healthcheck.js"]

CMD ["node", "dist/main.js"]
