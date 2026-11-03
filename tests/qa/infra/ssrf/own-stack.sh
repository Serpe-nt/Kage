#!/usr/bin/env bash
# own-stack.sh NAME NET [stack.sh up arguments ...]: brings up a stack of the
# ssrf topic's DNS world (overlay-dns.yaml on the /24 NET, e.g. 11.66.7),
# finishes it the way `stack.sh up` does when the wait is slow, and puts the
# world's names and the topic's zones on fast pacing. Sourced or run by a
# finding's repro.sh; `stack.sh down NAME` removes it.
set -uo pipefail
name="$1"; net="$2"; shift 2
s=/scripts/qa/stack.sh
bash "$s" up "$name" --overlay=/scripts/qa/infra/ssrf/overlay-dns.yaml "SSRF_NET=${net}" "$@" > "/tmp/own-stack-${name}.log" 2>&1
run="/target/qa/runs/${name}"
if [ ! -s "${run}/admin.token" ]; then
    for _ in $(seq 1 60); do
        c="$(bash "$s" cli "$name" persona list --json 2> /dev/null | jq '[.personas[] | select(.ready)] | length' 2> /dev/null)"
        [ "${c:-0}" = 3 ] && break
        sleep 3
    done
    bash "$s" cli "$name" key create --name qa-admin --scope admin --json | jq -r .token > "${run}/admin.token"
    chmod 600 "${run}/admin.token"
fi
p="$(bash "$s" policies "$name")"
cp /target/qa/kage/policies/domains.yaml "$p/domains.yaml"
bash "$s" fast "$name" > /dev/null
bash "$s" add-domains "$name" < /scripts/qa/infra/ssrf/fast-ssrf.yaml > /dev/null
echo "own stack ${name} is up on ${net}.0/24"
