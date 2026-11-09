#!/usr/bin/env bash
# QA round four, obs: a stack with the observability profile (collector with
# the debug exporter, Jaeger, Prometheus, Grafana on loopback).
#   up.sh NAME [GRAFANA_PORT] [extra stack.sh up arguments...]
set -uo pipefail
name="${1:?a stack name}"; shift
gport="${1:-13381}"; shift || true
pw="$(openssl rand -hex 16)"
bash /scripts/qa/stack.sh up "${name}" \
    --overlay=/scripts/qa/infra/timers/obs.yaml \
    --overlay=/scripts/qa/infra/obs/debug.yaml \
    KAGE_GRAFANA_ADMIN_PASSWORD="${pw}" KAGE_GRAFANA_PORT="${gport}" "$@"
