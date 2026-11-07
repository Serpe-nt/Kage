#!/usr/bin/env bash
# QA round two, topic tls: bring up a stack whose personas warm up on the
# world's https pages, with the CA in every profile before the first visit.
#   bash /scripts/qa/infra/tls/up-warm.sh NAME
# Programs the responder's warm-up pages as soon as tlsworld runs, runs
# trust.sh as soon as the personas exist (the first visit is 40 s or more
# after generation), and makes an admin key itself if `up` gave up waiting
# for warmed personas.
set -uo pipefail
name="${1:?name}"
run=/target/qa/runs/${name}
bash /scripts/qa/infra/tls/mkcerts.sh > /dev/null
bash /scripts/qa/stack.sh up "${name}" --overlay=/scripts/qa/infra/tls/compose.tls.yaml \
    --overlay=/scripts/qa/infra/tls/compose.tls-warm.yaml KAGE_PERSONAS_WARMUP=true > "/tmp/up-${name}.log" 2>&1 &
up=$!
page='<!doctype html><html lang="de"><head><title>Warm-up page</title></head><body><main><h1>Ein Artikel</h1><p>Ein ganz gewoehnlicher Artikel mit genug Text, damit eine Seite wie eine Seite aussieht und nicht wie eine leere Antwort oder eine Sperre.</p><p>Noch ein Absatz mit mehr Woertern, damit die Seite etwas Laenge hat und gelesen werden kann.</p></main></body></html>'
for _ in $(seq 1 120); do
    c=$(docker ps -q -f "name=qa-${name}-tlsworld-1")
    if [ -n "$c" ] && docker exec "$c" node -e "fetch('http://127.0.0.1:80/__log').then(r=>process.exit(r.ok?0:1),()=>process.exit(1))" 2> /dev/null; then
        jq -n --arg b "$page" '{path:"*",status:200,body:$b}' | docker exec -i "$c" node -e "let t='';process.stdin.on('data',(x)=>t+=x).on('end',()=>fetch('http://127.0.0.1:80/__set',{method:'POST',body:t}).then(r=>r.text()).then(console.log))"
        break
    fi
    sleep 2
done
for _ in $(seq 1 120); do
    n=$(bash /scripts/qa/stack.sh sql "${name}" "select count(*) from personas where retired_at is null" 2> /dev/null | tr -d ' ')
    if [ "${n:-0}" = 3 ]; then
        bash /scripts/qa/infra/tls/trust.sh "${name}"
        date -u +"trusted at %H:%M:%S"
        break
    fi
    sleep 2
done
wait "${up}"
tail -n 3 "/tmp/up-${name}.log"
if [ ! -s "${run}/admin.token" ]; then
    bash /scripts/qa/stack.sh cli "${name}" key create --name qa-admin --scope admin --json | jq -r .token > "${run}/admin.token"
    chmod 600 "${run}/admin.token"
    cat "${run}/admin.token" >> /target/qa/tokens.txt
fi
bash /scripts/qa/stack.sh key "${name}" "qa2-tls-adm-${RANDOM}" admin,fetch,jobs > "${run}/adminfetch.token"
echo "stack ${name} ready for tests"
