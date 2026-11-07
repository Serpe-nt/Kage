#!/usr/bin/env bash
# QA round three, lock: a second kage server `kage2` on the database of a
# stack that is up (round two's infra/pool/two-servers.sh, on this round's
# stack.sh, with `restart: "no"` so that a refused start stays refused).
#
#   bash /scripts/qa/infra/lock/second.sh NAME [define|start|stop|kill|wait|url]
#
# define  writes /target/qa/runs/NAME/extra.yaml: the stack's own `kage`
#         service as `docker compose config` resolves it, as `kage2`, its port
#         on KAGE_PUBLISH_PORT + 1000, its forwarder advertised as
#         http://kage2:8899, depends only on postgres. Starts nothing.
# start   docker compose up --detach --no-deps kage2 (prints nothing)
# wait    waits up to 180 s for kage2's /readyz 200 (exit 1 if not)
# stop|kill  docker compose stop / kill kage2
set -uo pipefail
name="${1:?a stack name}"; what="${2:-define}"
s=/scripts/qa/stack.sh
run=/target/qa/runs/${name}
# shellcheck disable=SC1091
source "${run}/env.sh"
port2=$((KAGE_PUBLISH_PORT + 1000))
case "${what}" in
define)
    rm -f "${run}/extra.yaml"
    bash $s dc "${name}" config --format json > "${run}/config.json" || exit 1
    jq --arg port "${port2}" '
      .services.kage
      | .ports = [{"target": 8080, "published": $port, "host_ip": "127.0.0.1", "protocol": "tcp"}]
      | .environment.KAGE__FORWARDER__ADVERTISE = "http://kage2:8899"
      | .environment.QA_SERVER = "kage2"
      | .restart = "no"
      | .depends_on = {"postgres": {"condition": "service_healthy"}}
      | del(.container_name)
      | {"services": {"kage2": .}}' "${run}/config.json" > "${run}/extra.yaml" || exit 1
    rm -f "${run}/config.json"
    chmod 600 "${run}/extra.yaml"
    ;;
start) bash $s dc "${name}" up --detach --no-deps kage2 > /dev/null 2>&1 ;;
wait)
    for _ in $(seq 1 90); do
        if curl --silent --fail --max-time 3 "http://127.0.0.1:${port2}/readyz" > /dev/null; then exit 0; fi
        sleep 2
    done
    exit 1
    ;;
url) echo "http://127.0.0.1:${port2}" ;;
stop|kill) bash $s dc "${name}" "${what}" kage2 ;;
*) echo "unknown ${what}" >&2; exit 2 ;;
esac
