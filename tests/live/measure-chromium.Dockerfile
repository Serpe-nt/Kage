# A measurement image for the idle windows of the live browser check
# (tests/live/browser-selftest.sh, KAGE_LIVE_IDLE_SECONDS): the Chromium
# sidecar image as it is deployed, with Chrome's NetLog on so that the hosts
# the browser asks for by itself can be listed, and for MODE=without with
# --disable-background-networking left to Chrome (ADR 0014). Built by the
# live script for one run and removed after it; never deployed, never
# pushed. Its build context is tests/live (the repository's .dockerignore
# lets nothing of tests/live into the images Kage ships).
ARG BASE
FROM ${BASE}
ARG MODE
USER root
COPY measure-switches.mjs /opt/kage-measure/measure-switches.mjs
RUN node /opt/kage-measure/measure-switches.mjs \
        /app/sidecar/packages/engine-chromium/dist/launch-plan.js "${MODE}"
USER 65532:65532
