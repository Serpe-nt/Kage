#!/usr/bin/env bash
# The QA round's harness: a compose stack of the end-to-end world per agent,
# in the closed fixture world (deploy/compose.e2e.yaml plus compose.qa.yaml).
# Run inside the container `kage-qa`:
#
#   (QA_PERSONAS=N as an argument of `up`: wait for N ready personas, default 3)
#   stack.sh up NAME [--overlay=FILE.yaml ...] [KAGE__SECTION__KEY=value | KAGE_E2E_X=value ...]   start, wait for 3 ready personas, make an admin key
#   stack.sh down NAME             save logs and /metrics to /out/qa/logs/NAME, remove the stack and its volumes
#   stack.sh restart NAME          docker compose restart kage, wait for /readyz
#   stack.sh url NAME              the API's base URL on this container's loopback
#   stack.sh admin NAME            the admin key's token
#   stack.sh key NAME KEYNAME SCOPES_CSV [extra json members, e.g. '"max_concurrent":1'] -> prints a token
#   stack.sh call NAME METHOD PATH [TOKEN] [JSON]   prints `HTTP <status>`, the headers, a blank line, the body
#   stack.sh cli NAME ARGS...      kage ARGS in the kage container
#   stack.sh dc NAME ARGS...       docker compose ... ARGS for this stack
#   stack.sh sql NAME "SQL"        psql in the stack's Postgres (no-align, tuples only)
#   stack.sh get NAME URL          GET a URL from inside the stack's network (fixture, world, hooks, responder)
#   stack.sh responder NAME JSON   program the responder, e.g. {"path":"/a","status":302,"headers":{"location":"http://x/"},"body":"","delay_ms":0}
#   stack.sh policies NAME         the directory of the stack's policy files (mounted read-only into kage)
#   stack.sh add-domains NAME      append domain rules from stdin to its domains.yaml (indented two spaces), then reload them
#   stack.sh fast NAME             add-domains: every name of the world reads at pacing fast, min_dwell 500ms, no sleeping hours
#   stack.sh logs NAME             dump the compose logs to /out/qa/logs/NAME/<service>.log and print the directory
#   stack.sh fw                    what the firewall dropped on its way out of the nested Docker (must stay 0)
#
# Nothing can leave: `fw-on` puts a DROP rule on every packet the nested
# Docker's containers send out of this container.
set -uo pipefail
repo="${QA_REPO:-/target/qa/kage}"
# This directory (tests/qa of the repository) is mounted at /scripts/qa.
runs=/target/qa/runs
mkdir -p "${runs}" /out/qa/logs

command_name="${1:?usage: stack.sh COMMAND NAME ...}"
shift

if [ "${command_name}" = fw-on ]; then
    iptables -C DOCKER-USER -o eth0 -j DROP 2> /dev/null && { echo "already on"; exit 0; }
    iptables -I DOCKER-USER -o eth0 -j DROP || exit 1
    iptables -I DOCKER-USER -o eth0 -j LOG --log-prefix 'QA-DROP ' || true
    # Round two: the container's own traffic has no way out either.
    iptables -C OUTPUT -o eth0 -j DROP 2> /dev/null || iptables -I OUTPUT -o eth0 -j DROP
    iptables -L DOCKER-USER -v -n
    iptables -L OUTPUT -v -n
    exit 0
fi
if [ "${command_name}" = fw ]; then
    iptables -L DOCKER-USER -v -n
    iptables -L OUTPUT -v -n
    dmesg 2> /dev/null | grep 'QA-DROP' | tail -n 20
    exit 0
fi

name="${1:?a stack name}"
shift
run="${runs}/${name}"
project="qa-${name}"
compose_files=(--file deploy/compose.yaml --file deploy/compose.e2e.yaml --file /scripts/qa/compose.qa.yaml)

load_run() {
    [ -f "${run}/env.sh" ] || { echo "no stack ${name}" >&2; exit 2; }
    # shellcheck disable=SC1090,SC1091
    source "${run}/env.sh"
    compose_files+=(--file "${run}/overlay.yaml")
    local extra
    for extra in "${run}"/overlay-*.yaml; do
        if [ -f "${extra}" ]; then compose_files+=(--file "${extra}"); fi
    done
    if [ -f "${run}/extra.yaml" ]; then compose_files+=(--file "${run}/extra.yaml"); fi
}
dc() {
    (cd "${repo}" && docker compose "${compose_files[@]}" --project-name "${project}" "$@")
}
base() {
    echo "http://127.0.0.1:${KAGE_PUBLISH_PORT}"
}
call() {
    local method="$1" path="$2" token="${3:-}" data="${4:-}" scratch
    scratch="$(mktemp -d)"
    local arguments=(--silent --show-error --max-time "${QA_MAX_TIME:-300}" --request "${method}"
        --dump-header "${scratch}/headers" --output "${scratch}/body" --write-out '%{http_code}')
    if [ -n "${token}" ]; then arguments+=(--header "Authorization: Bearer ${token}"); fi
    if [ -n "${data}" ]; then arguments+=(--header 'Content-Type: application/json' --data "${data}"); fi
    local status
    status="$(curl "${arguments[@]}" "$(base)${path}")"
    echo "HTTP ${status}"
    tr -d '\r' < "${scratch}/headers"
    cat "${scratch}/body"
    echo
    rm -rf "${scratch}"
}
in_world() {
    dc exec -T world node -e "$1"
}
dump_logs() {
    local out="/out/qa/logs/${name}"
    mkdir -p "${out}"
    for service in $(dc config --services 2> /dev/null || echo kage browser-chromium browser-firefox fixture world hooks proxy-a proxy-b proxy-c responder postgres migrate); do
        dc logs --no-color "${service}" > "${out}/${service}.log" 2>&1 || true
    done
    echo "${out}"
}

case "${command_name}" in
up)
    if [ -f "${run}/env.sh" ]; then echo "stack ${name} exists; down it first" >&2; exit 2; fi
    mkdir -p "${run}/policies"
    (cd "${repo}" && cp policies/*.yaml "${run}/policies/")
    chmod -R a+rX "${run}"
    port="$(flock /target/qa/port.lock bash -c 'p=$(cat /target/qa/port 2> /dev/null || echo 18100); echo $((p + 1)) > /target/qa/port; echo $((p + 1))')"
    {
        echo "export KAGE_PUBLISH_PORT=${port}"
        echo "export KAGE_WEBSHARE_API_KEY="
        echo "export KAGE_ABUSEIPDB_KEY="
        echo "export KAGE_PERSONAS_WARMUP=false"
        echo "export KAGE_E2E_POLICIES=${run}/policies"
        echo "export KAGE_E2E_SELFTEST_PAGES='[{id=\"probe\",kind=\"probe\",url=\"http://fixture.kage.test:8088/probe/\"}]'"
    } > "${run}/env.sh"
    {
        echo "services:"
        echo "  kage:"
        echo "    environment:"
        echo "      QA_STACK: ${name}"
    } > "${run}/overlay.yaml"
    for pair in "$@"; do
        key="${pair%%=*}"
        value="${pair#*=}"
        case "${key}" in
        --overlay)
            # Round two: a compose overlay of the caller's, added at up time.
            overlays=$(ls "${run}"/overlay-*.yaml 2> /dev/null | wc -l)
            cp "${value}" "${run}/overlay-$(printf '%02d' "${overlays}").yaml"
            ;;
        KAGE__*)
            quoted="$(VALUE="${value}" node -e 'console.log(JSON.stringify(process.env.VALUE))')"
            echo "      ${key}: ${quoted}" >> "${run}/overlay.yaml"
            ;;
        *)
            printf 'export %s=%q\n' "${key}" "${value}" >> "${run}/env.sh"
            ;;
        esac
    done
    load_run
    (cd "${repo}" && scripts/dev-secrets.sh > /dev/null)
    dc up --detach --wait --wait-timeout 300 || { echo "the stack did not come up" >&2; dc ps --all >&2; exit 1; }
    ready=0
    for _ in $(seq 1 60); do
        count="$(dc exec -T kage /usr/local/bin/kage persona list --json 2> /dev/null | jq '[.personas[] | select(.ready)] | length' 2> /dev/null || echo 0)"
        if [ "${count:-0}" = "${QA_PERSONAS:-3}" ]; then ready=1; break; fi
        sleep 3
    done
    [ "${ready}" = 1 ] || { echo "personas are not ready" >&2; dc exec -T kage /usr/local/bin/kage persona list --json >&2; exit 1; }
    dc exec -T kage /usr/local/bin/kage key create --name qa-admin --scope admin --json | jq -r .token > "${run}/admin.token"
    chmod 600 "${run}/admin.token"
    # Every token of the round, for the secrecy agent's grep of the logs. Kept
    # on the container's volume only.
    cat "${run}/admin.token" >> /target/qa/tokens.txt
    echo "stack ${name} is up at $(base)"
    ;;
down)
    load_run
    dump_logs > /dev/null
    if [ -f "${run}/admin.token" ]; then
        QA_MAX_TIME=20 call GET /metrics "$(cat "${run}/admin.token")" > "/out/qa/logs/${name}/metrics.txt" 2>&1 || true
    fi
    dc down --volumes --remove-orphans > /dev/null 2>&1
    rm -rf "${run}"
    echo "stack ${name} removed; logs in /out/qa/logs/${name}"
    ;;
restart)
    load_run
    dc restart kage
    for _ in $(seq 1 120); do
        if curl --silent --fail --max-time 3 "$(base)/readyz" > /dev/null; then echo ready; exit 0; fi
        sleep 2
    done
    echo "not ready after a restart" >&2
    exit 1
    ;;
url) load_run; base ;;
admin) cat "${run}/admin.token" ;;
key)
    load_run
    key_name="$1"
    scopes="$(printf '%s' "$2" | jq --raw-input --compact-output 'split(",")')"
    extra="${3:-}"
    QA_MAX_TIME=30 call POST /v1/admin/keys "$(cat "${run}/admin.token")" "{\"name\":\"${key_name}\",\"scopes\":${scopes}${extra:+,${extra}}}" | tail -n 2 | jq -r .token | tee -a /target/qa/tokens.txt
    ;;
call) load_run; call "$@" ;;
cli) load_run; dc exec -T kage /usr/local/bin/kage "$@" ;;
dc) load_run; dc "$@" ;;
sql) load_run; dc exec -T postgres psql --username kage --dbname kage --no-align --tuples-only --command "$1" ;;
get)
    load_run
    in_world "fetch('$1').then((r) => r.text()).then((t) => console.log(t), (e) => { console.error(String(e)); process.exit(1); })"
    ;;
responder)
    load_run
    printf '%s' "$1" | dc exec -T world node -e "let t='';process.stdin.on('data',(c)=>t+=c).on('end',()=>fetch('http://qa.kage.test:8091/__set',{method:'POST',body:t}).then((r)=>r.text()).then(console.log,(e)=>{console.error(String(e));process.exit(1)}))"
    ;;
policies) load_run; echo "${run}/policies" ;;
add-domains)
    load_run
    file="${run}/policies/domains.yaml"
    if grep --quiet '^domains: {}$' "${file}"; then
        sed 's/^domains: {}$/domains:/' "${file}" > "${file}.tmp"
        cat "${file}.tmp" > "${file}"
        rm -f "${file}.tmp"
    fi
    cat >> "${file}"
    chmod -R a+rX "${run}/policies"
    QA_MAX_TIME=30 call POST /v1/admin/policies/reload "$(cat "${run}/admin.token")" | head -n 1
    ;;
fast)
    # Through bash, so that it works when this script was run as `bash
    # stack.sh` from a copy without the executable bit (F198).
    bash "${BASH_SOURCE[0]}" add-domains "${name}" < /scripts/qa/fast-domains.yaml
    ;;
logs) load_run; dump_logs ;;
*)
    echo "unknown command ${command_name}" >&2
    exit 2
    ;;
esac
