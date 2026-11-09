#!/usr/bin/env bash
# QA round four (F235 around): round F core's flows-04 script with an engine and a
# step after the click. Three flows: goto a page of FIRST, click to a page of LATER
# that answers after 40 s, then AFTER (none, assert, waiturl); then whether LATER
# is paused (its next page held about 30 s) and FIRST is not.
# usage: bash f235p.sh STACK ENGINE [AFTER]   (a stack that is up, `stack.sh fast` applied)
set -uo pipefail
STACK="${1:?stack}"
ENGINE="${2:?engine}"
AFTER="${3:-none}"
case "$AFTER" in
    assert) X=',{"action":"assert","text_contains":"Words a reader reads.","timeout_ms":5000}' ;;
    waiturl) X=',{"action":"wait_for","url":"**/x*","timeout_ms":5000}' ;;
    *) X="" ;;
esac
H=/scripts/qa/stack.sh
BASE="$(bash $H url "$STACK")"
u="qa3-f04-$$-$RANDOM"
T="$(bash $H key "$STACK" "$u" fetch,jobs '"max_concurrent":50,"requests_per_minute":6000')"
api() { if [ -n "${3:-}" ]; then curl -s --max-time 200 -X "$1" -H "Authorization: Bearer $T" -H 'Content-Type: application/json' --data "$3" "$BASE$2"; else curl -s --max-time 60 -X "$1" -H "Authorization: Bearer $T" "$BASE$2"; fi; }
resp() { bash $H responder "$STACK" "$1" > /dev/null; }
log() { bash $H get "$STACK" "http://qa.kage.test:8091/__log"; }
FIRST=other.qa.test   # the flow's first site: it answers at once
LATER=qa.kage.test    # the site the click leads to: it answers after 40 s
html() { jq -cn --arg h "$1" --arg p "$2" --arg b "$3" --argjson d "${4:-0}" '{host:$h,path:$p,status:200,headers:{"content-type":"text/html"},body:$b,delay_ms:$d}'; }
for i in 1 2 3; do
    resp "$(html "$LATER" "/$u/x$i" "<html><body><main><h1>x$i</h1><p>Words a reader reads.</p></main></body></html>" 40000)"
    resp "$(html "$FIRST" "/$u/a$i" "<html><body><main><h1>first</h1><p>Words a reader reads on this page, long enough to be content.</p><a id=go href='http://$LATER:8091/$u/x$i' style='display:block;width:300px;height:80px'>on</a></main></body></html>")"
done
for h in "$FIRST" "$LATER"; do resp "$(html "$h" "/$u/after" "<html><body><main><h1>after</h1><p>Words a reader reads.</p></main></body></html>")"; done
for i in 1 2 3; do
    body="{\"type\":\"flow\",\"level\":2,\"max_retries\":0,\"persona\":{\"engines\":[\"$ENGINE\"]},\"actions\":[{\"action\":\"goto\",\"url\":\"http://$FIRST:8091/$u/a$i\"},{\"action\":\"click\",\"selector\":\"#go\",\"timeout_ms\":5000}$X]}"
    [ "$i" = 1 ] && echo "POST /v1/jobs $body  (x3)"
    jid="$(api POST /v1/jobs "$body" | jq -r .id)"
    for _ in $(seq 1 90); do s="$(api GET "/v1/jobs/$jid" | jq -r .state)"; case "$s" in succeeded|partial|failed|cancelled) break;; esac; sleep 2; done
    echo "$(date -u +%T.%3N) flow $i: $s, its request: $(api GET "/v1/jobs/$jid/items" | jq -c '.items[-1].result | {outcome, reason, status, final_url}')"
done
ended="$(date +%s)"
for h in "$FIRST" "$LATER"; do
    jid="$(api POST /v1/jobs "{\"type\":\"batch\",\"urls\":[\"http://$h:8091/$u/after\"],\"defaults\":{\"level\":2,\"max_retries\":0}}" | jq -r .id)"
    echo "$(date -u +%T.%3N) POST /v1/jobs a batch of http://$h:8091/$u/after"
done
sleep 45
echo "the site got:"; log | jq -r --arg u "/$u/" '.[] | select(.url | startswith($u)) | "  \(.at) \(.host)\(.url)"'
at() { log | jq -r --arg h "$1" --arg p "/$u/after" '[.[] | select(.host == $h and .url == $p) | .at][0] // ""'; }
f="$(at "$FIRST")"; l="$(at "$LATER")"
fd=$(( $(date -d "$f" +%s 2> /dev/null || echo 99999999999) - ended )); ld=$(( $(date -d "$l" +%s 2> /dev/null || echo 99999999999) - ended ))
echo "after the third flow ended: the first site's next page was asked after ${fd} s, the later site's after ${ld} s"
if [ "$fd" -ge 15 ] && [ "$ld" -le 10 ]; then echo REPRODUCED; exit 0; fi
echo NOT-REPRODUCED; exit 1
