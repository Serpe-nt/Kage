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
# What an engine ships besides its compiled code (both engines: the wrapper
# that starts the browser under a CPU affinity mask). Test support is
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

# Camoufox, pinned by version, by release and by the checksum of the release
# archive (docs/versions.md, fingerprint-critical). The Firefox engine
# refuses to start on any other build. The archive is fetched by its exact
# address, never through the launcher's own download, which installs
# whatever build it resolves. A bump is a dedicated change: it moves the
# build the engine names (packages/engine-firefox/src/host.ts) and
# engines.firefox.browser_major, and re-runs the parity suite and the
# self-tests (docs/runbooks/engine-firefox.md).
# `scripts/setup-camoufox.sh` reads the same three lines for CI runners and
# developer machines, so tests and image run the same build.
ARG CAMOUFOX_VERSION=152.0.4
ARG CAMOUFOX_RELEASE=beta.30
ARG CAMOUFOX_SHA256=5720d45b894ce1770543de024c6f10d514b38be560fa2dc3226b3d8586caf672

# The Juggler backport (ADR 0010). The driver inside this build runs every
# evaluation of a page as user input, so a page nobody touched reads user
# activation; upstream took that out in the builds from v156.0.1-beta.32 on.
# `scripts/patch-camoufox-juggler.mjs` takes the same four statements out of
# this one, inside its `omni.ja`: the first checksum is that archive as the
# release ships it, the second what the patch must turn it into. The engine
# refuses to start on any other archive
# (packages/engine-firefox/src/host.ts names the second one too). Both lines
# go when the build moves to one that has the fix;
# `scripts/setup-camoufox.sh` reads them as well.
ARG CAMOUFOX_OMNI_SHA256=269da6dadc2dd93b5b0baafecccc8de1849020d04889d4fdaaa5423bf7e8545e
ARG CAMOUFOX_OMNI_PATCHED_SHA256=ad3799d8c96fa80ba26a58cb1aaa28f525801d9ecc727a3e2be9262318ccbf5c

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
#   firefox   Camoufox, headful on Xvfb as well, with Mesa for its pixels.
#             The build is complete but for the libraries a Firefox links
#             against: GTK 3, the X client libraries, ALSA and D-Bus. The
#             list is what its binaries name as needed, and the step fails
#             when the browser cannot load one. No font package is
#             installed for it: the build brings the fonts of the three
#             systems a persona may claim and each persona's fontconfig
#             file picks among them.
#   both      setxkbmap (x11-xkb-utils) gives a persona's display the
#             keyboard layout of its locale. PulseAudio is the sound card a
#             container does not have: the sidecar starts one with an
#             output and an input that go nowhere (SIDECAR_AUDIO below), so
#             that a page finds media devices as on a desktop.
#
# Both variants record what they run under /etc/kage/.
#
# The Camoufox archive (663 MB) lives in a build cache, never in a layer: a
# rebuild of this step does not download it again, and a copy from the
# cache is checked against the pin like a fresh one. The cache is locked
# while a build uses it, so two builds never write the archive at once. The
# patch tool is read from the build context and is not part of the image.
RUN --mount=type=cache,id=kage-camoufox,target=/var/cache/kage,sharing=locked \
    --mount=type=bind,source=scripts/patch-camoufox-juggler.mjs,target=/tmp/patch-camoufox-juggler.mjs \
    install --directory /etc/kage \
    && printf '%s\n' "${ENGINE}" > /etc/kage/engine \
    && node --version > /etc/kage/node-version \
    && common="xvfb x11-xkb-utils libgl1-mesa-dri libegl1 pulseaudio" \
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
            /tmp/chrome.deb ${common} tzdata ${fonts} \
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
        && dpkg-query --show --showformat '${Package} ${Version}\n' ${fonts} > /etc/kage/fonts; \
    else \
        libraries="libasound2t64 libatk1.0-0t64 libcairo-gobject2 libcairo2 libdbus-1-3 libfontconfig1 \
            libfreetype6 libgdk-pixbuf-2.0-0 libglib2.0-0t64 libgtk-3-0t64 libpango-1.0-0 \
            libpangocairo-1.0-0 libx11-6 libx11-xcb1 libxcb-shm0 libxcb1 libxcomposite1 libxcursor1 \
            libxdamage1 libxext6 libxfixes3 libxi6 libxrandr2 libxrender1" \
        && build="${CAMOUFOX_VERSION}-${CAMOUFOX_RELEASE}" \
        && archive="/var/cache/kage/camoufox-${build}-lin.x86_64.zip" \
        && apt-get update \
        && apt-get install --yes --no-install-recommends ca-certificates curl unzip \
        && if ! echo "${CAMOUFOX_SHA256}  ${archive}" | sha256sum --check --strict --status 2> /dev/null; then \
            curl --fail --silent --show-error --location --retry 3 --output "${archive}" \
                "https://github.com/daijro/camoufox/releases/download/v${build}/camoufox-${build}-lin.x86_64.zip"; \
        fi \
        && echo "${CAMOUFOX_SHA256}  ${archive}" | sha256sum --check --strict \
        && unzip -q "${archive}" -d /opt/camoufox \
        # The Juggler backport: fails the build unless the archive is the
        # pinned one and the result the pinned result.
        && node /tmp/patch-camoufox-juggler.mjs /opt/camoufox \
            --from "${CAMOUFOX_OMNI_SHA256}" --to "${CAMOUFOX_OMNI_PATCHED_SHA256}" \
        && chmod -R a+rX,go-w /opt/camoufox \
        # The engine reads which build this is from this file and refuses
        # any other (packages/engine-firefox/src/host.ts).
        && printf '{"version":"%s","release":"%s"}\n' "${CAMOUFOX_VERSION}" "${CAMOUFOX_RELEASE}" \
            > /opt/camoufox/version.json \
        && apt-get install --yes --no-install-recommends ${common} ${libraries} \
        && apt-get purge --yes curl unzip \
        && apt-get autoremove --yes \
        && rm -rf /var/lib/apt/lists/* \
        # Loads the whole browser library, so a missing system library
        # fails the build here and not the first context in production.
        && installed="$(HOME=/nonexistent /opt/camoufox/camoufox-bin --version | sed -E 's/^.* //')" \
        && test "${installed}" = "${build}" \
        && printf '%s\n' "${installed}" > /etc/kage/camoufox-version \
        && printf '%s\n' "${CAMOUFOX_VERSION}" > /etc/kage/browser-version; \
    fi \
    && dpkg-query --show --showformat '${Package} ${Version}\n' xvfb x11-xkb-utils libgl1-mesa-dri > /etc/kage/display \
    && dpkg-query --show --showformat '${Package} ${Version}\n' pulseaudio > /etc/kage/audio

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

# SIDECAR_AUDIO=pulse: the sidecar starts the image's PulseAudio for its
# browsers, with its socket and state below /tmp. `none` runs without one.
ENV NODE_ENV=production \
    ENGINE=${ENGINE} \
    SIDECAR_HOST=0.0.0.0 \
    SIDECAR_PORT=8090 \
    SIDECAR_PERSONAS_DIR=/var/lib/kage/personas \
    SIDECAR_ARTIFACTS_DIR=/var/lib/kage/artifacts \
    SIDECAR_AUDIO=pulse
USER 65532:65532
WORKDIR /app/sidecar/packages/server
EXPOSE 8090

HEALTHCHECK --interval=10s --timeout=5s --start-period=30s --retries=3 \
    CMD ["node", "dist/healthcheck.js"]

CMD ["node", "dist/main.js"]
