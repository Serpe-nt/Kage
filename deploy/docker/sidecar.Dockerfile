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
ARG ENGINE
COPY sidecar/pnpm-lock.yaml sidecar/pnpm-workspace.yaml sidecar/tsconfig.base.json sidecar/tsconfig.json ./
COPY sidecar/packages ./packages
RUN --mount=type=cache,id=kage-pnpm-store,target=/pnpm/store \
    pnpm install --frozen-lockfile --store-dir /pnpm/store
RUN pnpm run build
# What an engine ships besides its compiled code (the Chromium engine: the
# wrapper that starts Chrome under a CPU affinity mask). Test support is
# compiled next to the code and does not belong in the image.
RUN mkdir -p /stage/bin \
    && if [ -d "packages/engine-${ENGINE}/bin" ]; then cp -a "packages/engine-${ENGINE}/bin/." /stage/bin/; fi \
    && chmod -R 0755 /stage/bin \
    && rm -rf packages/*/dist/testing

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
COPY --from=build /stage/bin packages/engine-${ENGINE}/bin
COPY --from=build /app/sidecar/packages/server/dist packages/server/dist

# --- runtime ---------------------------------------------------------------
FROM ${NODE_IMAGE} AS runtime
ARG ENGINE

# Chrome stable, pinned by version and by the checksum of Google's package
# (docs/versions.md, fingerprint-critical). A bump is a dedicated change:
# it moves engines.chromium.browser_major, re-records the browser capture
# (`just capture-browser-fingerprint`) and re-runs the self-tests.
# `scripts/setup-chrome.sh` reads the same two lines for CI runners and
# developer machines, so tests and image run the same build.
ARG CHROME_VERSION=155.0.8059.39
ARG CHROME_SHA256=c58aa0f2cd66179c9f050e062c882d27aa9b9f8c2b7c73fee3498560b5ed0b38

LABEL org.opencontainers.image.title="kage-browser-${ENGINE}" \
      org.opencontainers.image.description="Kage browser sidecar, ${ENGINE} engine" \
      org.opencontainers.image.source="https://github.com/Serpe-nt/Kage" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"

# Engine-specific system packages and the browser itself.
#
#   chromium  Real Chrome, headful on Xvfb (D3). Mesa gives the browser a
#             software OpenGL stack, so WebGL exists and names llvmpipe, as a
#             Linux machine without GPU acceleration does. The fonts are the
#             ones a Debian 13 GNOME desktop installs (the packages
#             `task-gnome-desktop` pulls in, read from the distribution's own
#             metadata), not a hand-picked set: font enumeration looks like a
#             desktop of the distribution the image is built on.
#   firefox   A stub until M5 fetches the pinned Camoufox build.
#
# Both variants record what they run under /etc/kage/.
RUN install --directory /etc/kage \
    && printf '%s\n' "${ENGINE}" > /etc/kage/engine \
    && node --version > /etc/kage/node-version \
    && if [ "${ENGINE}" = "chromium" ]; then \
        fonts="fonts-cantarell fonts-dejavu fonts-dejavu-core fonts-dejavu-extra fonts-dejavu-mono \
            fonts-droid-fallback fonts-liberation fonts-liberation-sans-narrow fonts-noto-color-emoji \
            fonts-noto-mono fonts-opensymbol fonts-quicksand fonts-symbola fonts-urw-base35" \
        && apt-get update \
        && apt-get install --yes --no-install-recommends ca-certificates curl \
        && curl --fail --silent --show-error --location --retry 3 --output /tmp/chrome.deb \
            "https://dl.google.com/linux/chrome/deb/pool/main/g/google-chrome-stable/google-chrome-stable_${CHROME_VERSION}-1_amd64.deb" \
        && echo "${CHROME_SHA256}  /tmp/chrome.deb" | sha256sum --check --strict \
        && apt-get install --yes --no-install-recommends \
            /tmp/chrome.deb xvfb libgl1-mesa-dri libegl1 tzdata ${fonts} \
        && apt-get purge --yes curl \
        && apt-get autoremove --yes \
        && rm -rf /tmp/chrome.deb /var/lib/apt/lists/* /etc/apt/sources.list.d/google-chrome.list \
            /etc/cron.daily/google-chrome /opt/google/chrome/cron \
        # fontconfig trusts its cache only while a font directory's
        # modification time is the one the cache recorded. An image layer
        # keeps whole seconds, so the times are made whole before the cache
        # is written; otherwise every process would find the cache stale and
        # scan the fonts again, in a container that cannot rewrite it.
        && find /usr/share/fonts /usr/local/share/fonts -type d -exec touch --date "@$(date +%s)" {} + \
        && fc-cache --force --system-only \
        && installed="$(/opt/google/chrome/chrome --version | sed -E 's/^[^0-9]*([0-9.]+).*$/\1/')" \
        && test "${installed}" = "${CHROME_VERSION}" \
        && printf '%s\n' "${installed}" > /etc/kage/chrome-version \
        && printf '%s\n' "${installed}" > /etc/kage/browser-version \
        && dpkg-query --show --showformat '${Package} ${Version}\n' ${fonts} > /etc/kage/fonts \
        && dpkg-query --show --showformat '${Package} ${Version}\n' xvfb libgl1-mesa-dri > /etc/kage/display; \
    else \
        printf '%s\n' "none (engine stub: the browser is installed from M5 on)" > /etc/kage/browser-version; \
    fi

# The runtime needs node only: drop the package managers. The user has the
# same numeric identity as in the kage image, so both can share the artifacts
# volume.
RUN rm -rf /usr/local/lib/node_modules /usr/local/bin/npm /usr/local/bin/npx \
        /usr/local/bin/corepack /usr/local/bin/yarn /usr/local/bin/yarnpkg /opt/yarn-* \
    && groupadd --gid 65532 kage \
    && useradd --uid 65532 --gid 65532 --no-create-home --home-dir /nonexistent \
        --shell /usr/sbin/nologin kage \
    && install --directory --owner kage --group kage --mode 0750 \
        /var/lib/kage/personas /var/lib/kage/artifacts

COPY --from=deps /app/sidecar /app/sidecar

ENV NODE_ENV=production \
    ENGINE=${ENGINE} \
    SIDECAR_HOST=0.0.0.0 \
    SIDECAR_PORT=8090 \
    SIDECAR_PERSONAS_DIR=/var/lib/kage/personas \
    SIDECAR_ARTIFACTS_DIR=/var/lib/kage/artifacts
USER 65532:65532
WORKDIR /app/sidecar/packages/server
EXPOSE 8090

HEALTHCHECK --interval=10s --timeout=5s --start-period=30s --retries=3 \
    CMD ["node", "dist/healthcheck.js"]

CMD ["node", "dist/main.js"]
