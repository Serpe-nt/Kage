#!/usr/bin/env bash
# QA round three, api: a kage server with no proxy pool, beside a stack that is up.
#
#   bash /scripts/qa/infra/api/nopool.sh NAME [up|url|down]
#
# up    creates the database `kage_np` in the stack's Postgres, renders the
#       stack's own `kage` service (docker compose config) as `kagenp` into
#       /target/qa/runs/NAME/overlay-np.yaml with that database, an empty
#       static list, no metrics listener and no telemetry, migrates it with
#       `kage migrate`, starts it on KAGE_PUBLISH_PORT + 2000 and waits for
#       /healthz (its /readyz is 503: no persona). Then it makes an admin key
#       on it and writes it to /target/qa/runs/NAME/np-admin.token.
# url   its base URL.  down   stops and removes kagenp and its overlay.
set -uo pipefail
name="${1:?a stack name}"; what="${2:-up}"
s=/scripts/qa/stack.sh
run=/target/qa/runs/${name}
# shellcheck disable=SC1091
source "${run}/env.sh"
port3=$((KAGE_PUBLISH_PORT + 2000))
case "${what}" in
up)
    bash $s sql "${name}" "select 1" > /dev/null || exit 1
    bash $s dc "${name}" exec -T postgres psql --username kage --dbname kage -c "create database kage_np" > /dev/null 2>&1
    bash $s dc "${name}" config --format json > "${run}/np-config.json" || exit 1
    jq --arg port "${port3}" '
      .services.kage
      | .ports = [{"target": 8080, "published": $port, "host_ip": "127.0.0.1", "protocol": "tcp"}]
      | .environment.KAGE__DATABASE__URL |= sub("/kage$"; "/kage_np")
      | .environment.KAGE__PROXIES__STATIC_LIST = ""
      | .environment.KAGE__PROXIES__STATIC_LIST_FILE = ""
      | .environment.KAGE__FORWARDER__ADVERTISE = "http://kagenp:8899"
      | del(.environment.KAGE__SERVER__METRICS_BIND)
      | del(.environment.KAGE__TELEMETRY__OTLP_ENDPOINT)
      | .environment.QA_SERVER = "kagenp"
      | .depends_on = {"postgres": {"condition": "service_healthy"}}
      | del(.container_name)
      | {"services": {"kagenp": .}}' "${run}/np-config.json" > "${run}/overlay-np.yaml" || exit 1
    rm -f "${run}/np-config.json"
    chmod 600 "${run}/overlay-np.yaml"
    bash $s dc "${name}" run --rm --no-deps -T kagenp migrate > /dev/null 2>&1 || { echo "migrate failed" >&2; exit 1; }
    bash $s dc "${name}" up --detach --no-deps kagenp > /dev/null 2>&1 || { echo "kagenp did not start" >&2; exit 1; }
    for _ in $(seq 1 60); do
        if curl --silent --fail --max-time 3 "http://127.0.0.1:${port3}/healthz" > /dev/null; then
            bash $s dc "${name}" exec -T kagenp /usr/local/bin/kage key create --name qa-np-admin --scope admin --json 2> /dev/null | jq -r .token > "${run}/np-admin.token"
            chmod 600 "${run}/np-admin.token"
            echo "kagenp is up at http://127.0.0.1:${port3}"; exit 0
        fi
        sleep 2
    done
    echo "kagenp is not up" >&2; exit 1
    ;;
url) echo "http://127.0.0.1:${port3}" ;;
down)
    bash $s dc "${name}" rm --stop --force kagenp > /dev/null 2>&1
    rm -f "${run}/overlay-np.yaml" "${run}/np-admin.token"
    echo "kagenp removed"
    ;;
*) echo "unknown ${what}" >&2; exit 2 ;;
esac
