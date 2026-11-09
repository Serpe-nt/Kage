#!/usr/bin/env bash
# Helpers for an obs stack, from inside the container (round two's timers/obs.sh).
#   ST=NAME obs.sh prom 'QUERY'       instant query against Prometheus
#   ST=NAME obs.sh promget '/api/v1/...'
#   ST=NAME obs.sh jaeger '/api/...'  Jaeger query API (16686)
#   ST=NAME obs.sh grafana '/api/...' Grafana API as admin on loopback
#   ST=NAME obs.sh metrics            /metrics as the admin key
#   ST=NAME obs.sh collector          the collector's stdout (debug exporter)
st="${ST:?ST=stack}"; H=/scripts/qa/stack.sh
case "$1" in
prom) q=$(jq -rn --arg q "$2" '$q|@uri'); bash $H get "$st" "http://prometheus:9090/api/v1/query?query=${q}" ;;
promget) bash $H get "$st" "http://prometheus:9090$2" ;;
jaeger) bash $H get "$st" "http://jaeger:16686$2" ;;
grafana)
  source /target/qa/runs/$st/env.sh
  curl -s -u "admin:${KAGE_GRAFANA_ADMIN_PASSWORD}" "${@:3}" "http://127.0.0.1:${KAGE_GRAFANA_PORT}$2" ;;
metrics) bash $H call "$st" GET /metrics "$(bash $H admin "$st")" | sed '1,/^$/d' ;;
collector) bash $H dc "$st" logs --no-color otel-collector 2>/dev/null ;;
esac
