#!/usr/bin/env bash
# QA round two, pool: a second kage server on the database of a stack that
# is up (spec §16.6 "by the same server after a restart or by another one").
#
#   bash /scripts/qa/infra/pool/two-servers.sh NAME [up|url|stop|kill|start]
#
# up    writes /target/qa/runs/NAME/extra.yaml with a service `kage2`: the
#       stack's own `kage` service as `docker compose config` resolves it
#       (same image, environment, volumes, networks and sidecars), its port
#       published on KAGE_PUBLISH_PORT + 1000, its forwarder advertised to
#       the sidecars as http://kage2:8899; then starts it and waits for
#       /readyz. Nothing of the first server changes.
# url   the second server's base URL.
# stop|kill|start  docker compose stop / kill / start of kage2.
set -uo pipefail
name="${1:?a stack name}"; what="${2:-up}"
s=/scripts/qa/infra/pool/stack.sh
run=/target/qa/runs/${name}
# shellcheck disable=SC1091
source "${run}/env.sh"
port2=$((KAGE_PUBLISH_PORT + 1000))
case "${what}" in
up)
    bash $s dc "${name}" config --format json > "${run}/config.json" || exit 1
    jq --arg port "${port2}" '
      .services.kage
      | .ports = [{"target": 8080, "published": $port, "host_ip": "127.0.0.1", "protocol": "tcp"}]
      | .environment.KAGE__FORWARDER__ADVERTISE = "http://kage2:8899"
      | .environment.QA_SERVER = "kage2"
      | .depends_on = {"postgres": {"condition": "service_healthy"}}
      | del(.container_name)
      | {"services": {"kage2": .}}' "${run}/config.json" > "${run}/extra.yaml" || exit 1
    rm -f "${run}/config.json"
    chmod 600 "${run}/extra.yaml"
    bash $s dc "${name}" up --detach --no-deps kage2 > /dev/null 2>&1 || { echo "kage2 did not start" >&2; exit 1; }
    for _ in $(seq 1 90); do
        if curl --silent --fail --max-time 3 "http://127.0.0.1:${port2}/readyz" > /dev/null; then echo "kage2 is up at http://127.0.0.1:${port2}"; exit 0; fi
        sleep 2
    done
    echo "kage2 is not ready" >&2; exit 1
    ;;
url) echo "http://127.0.0.1:${port2}" ;;
stop|kill|start) bash $s dc "${name}" "${what}" kage2 ;;
*) echo "unknown ${what}" >&2; exit 2 ;;
esac
