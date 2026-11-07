#!/usr/bin/env bash
# Pulls every image a world of round three needs, before the firewall goes on.
set -uo pipefail
for image in \
    'postgres:17.11-trixie@sha256:2d2b8998d31037bf721cfdf764d76ba74171b4fab3431b7f72c27c56ddbdf9e3' \
    'otel/opentelemetry-collector:0.162.0@sha256:310a800ad69ee430e7c541796852a242c9c7db97aaad4daa5ccf843c525fbdb2' \
    'jaegertracing/jaeger:2.20.0@sha256:46a886260e04002d8f45e213fc39063fa11a50446048fdaa64786fc0840cb9f8' \
    'prom/prometheus:v3.13.4-distroless@sha256:f11bbfb615cf24aa9ef28d2cb527ef81e3bcaa1a0ff13c0ec5b11edd9b351bdc' \
    'grafana/grafana:13.2.4@sha256:dbbc38ea21103cefb50396340cd88afec779c7e070b0a7bf5a75b4ce46651d3f' \
    'grafana/k6:2.3.0@sha256:9c2dee7f8ed74d317e4027c06a10f169b625638189de8d4555d0b3486a5aeb34' \
    'ghcr.io/serpe-nt/kage:2.0.2' \
    'ghcr.io/serpe-nt/kage-browser-chromium:2.0.2' \
    'ghcr.io/serpe-nt/kage:2.0.4' \
    'ghcr.io/serpe-nt/kage-browser-chromium:2.0.4'; do
    if docker pull --quiet "${image}"; then echo "pulled ${image}"; else echo "FAILED ${image}"; fi
done
docker images --digests --format '{{.Repository}}:{{.Tag}} {{.Digest}}'
