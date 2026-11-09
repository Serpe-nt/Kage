#!/usr/bin/env bash
# QA round four: a human step that runs out of its `timeout_ms`, then the
# next request of the same persona. Flow A (in a session) opens keys4.html
# and runs one step of KIND with a budget too short for it; flow B, right
# after in the same session, opens the page and only waits. B's page must
# hear no key, wheel or click of A's (spec §10.5, F96; the review of #82).
#   bash tto.sh STACK ENGINE KIND [BUDGET_MS]
# KIND: type (180 characters), typelent (a text of lent characters), select
# (an option 150 down a long select), scroll (to the end of a tall page),
# press (one key), click, hover. Prints one JSON line per run.
# CROSS=1: flow B is another key's, without a session, the same engine (the
# same persona when the stack has one per engine); B's page opens with its
# input focused, so a stray Enter submits its form (submitsB). WAITB: how long
# flow B waits on its page (ms, default 8000).
set -uo pipefail
STACK="${1:?stack}"
E="${2:?engine}"
KIND="${3:?kind}"
BUDGET="${4:-3000}"
# shellcheck disable=SC1091
source /scripts/qa/infra/browser/lib.sh "$STACK"
T="$(tok "qa4b-tto-$(uid)" fetch,jobs,sessions)"
id="tto$(uid)"
page keys4.html "/$id/keys"
TEXT="$(printf 'the quick brown fox jumps over the lazy dog %.0s' 1 2 3 4)"
LENT="жзийклмнопрстуфхцчшщъыьэюя жзийклмнопрстуфхцчшщ"
case "$KIND" in
type) step=$(jq -nc --arg t "$TEXT" --argjson b "$BUDGET" '{action:"type",selector:"#i",text:$t,timeout_ms:$b}') ;;
typelent) step=$(jq -nc --arg t "$LENT" --argjson b "$BUDGET" '{action:"type",selector:"#i",text:$t,timeout_ms:$b}') ;;
select) step=$(jq -nc --argjson b "$BUDGET" '{action:"select",selector:"#s",value:"option 150",timeout_ms:$b}') ;;
scroll) step=$(jq -nc --argjson b "$BUDGET" '{action:"scroll",depth:1,timeout_ms:$b}') ;;
press) step=$(jq -nc --argjson b "$BUDGET" '{action:"press",key:"Enter",timeout_ms:$b}') ;;
click) step=$(jq -nc --argjson b "$BUDGET" '{action:"click",selector:"#sub",timeout_ms:$b}') ;;
hover) step=$(jq -nc --argjson b "$BUDGET" '{action:"hover",selector:"#sub",timeout_ms:$b}') ;;
*) echo "unknown kind $KIND" >&2; exit 2 ;;
esac
sid="$id-$E"
api POST /v1/sessions "$T" "$(jq -nc --arg s "$sid" --arg e "$E" '{id:$s,persona:{engines:[$e]},domain:"qa.kage.test"}')" > /dev/null
a=$(jq -nc --arg u "http://qa.kage.test:8091/$id/keys?options=300&$E-a" --argjson s "$step" '[{action:"goto",url:$u},{action:"wait_for",selector:"#i"},$s]')
ja=$(api POST /v1/jobs "$T" "$(jq -nc --arg s "$sid" --argjson a "$a" '{type:"flow",level:3,max_retries:0,session_id:$s,behavior:{dwell_ms:500},timeout_ms:120000,actions:$a}')" | jq -r .id)
t0=$(date +%s%N)
sta=$(wait_job "$T" "$ja")
ta=$((($(date +%s%N) - t0) / 1000000))
itemsa=$(api GET "/v1/jobs/$ja/items" "$T")
b=$(jq -nc --arg w "${WAITB:-8000}" --arg u "http://qa.kage.test:8091/$id/keys?focus=1&$E-b" '[{action:"goto",url:$u},{action:"wait_for",selector:"#i"},{action:"wait_for",ms:($w|tonumber)}]')
TB="$T"; bs="$sid"
if [ "${CROSS:-0}" = 1 ]; then TB="$(tok "qa4b-ttob-$(uid)" fetch,jobs)"; bs=""; fi
jb=$(api POST /v1/jobs "$TB" "$(jq -nc --arg s "$bs" --arg e "$E" --argjson b "$b" '{type:"flow",level:3,max_retries:0} + (if $s == "" then {persona:{engines:[$e]}} else {session_id:$s} end) + {behavior:{dwell_ms:500},timeout_ms:120000,include:["dom"],actions:$b}')" | jq -r .id)
stb=$(wait_job "$TB" "$jb")
ab=$(api GET "/v1/jobs/$jb/items" "$TB" | jq -r '.items[-1].result.artifacts.dom_html // empty')
dom=""
[ -n "$ab" ] && dom=$(art "$TB" "$ab")
mkdir -p "${OUT:-/tmp/tto}" && printf %s "$dom" > "${OUT:-/tmp/tto}/$E-$KIND-cross${CROSS:-0}-$id.html"
subs=$(printf %s "$dom" | sed -n 's/.*<pre id="sub">\([0-9]*\)<.*/\1/p' | head -1)
valb=$(printf %s "$dom" | sed -n 's/.*<pre id="vi">\([^<]*\)<.*/\1/p' | head -1)
printf %s "$dom" | jq -Rs '{dom_html:.}' | node /scripts/qa/infra/browser/keys4-analyze.mjs "" "" > "${OUT:-/tmp/tto}/analysis-$id.json"
r=$(cat "${OUT:-/tmp/tto}/analysis-$id.json")
echo "$r" | jq -c --arg e "$E" --arg k "$KIND" --arg b "$BUDGET" --arg sta "$sta" --arg stb "$stb" --argjson ta "$ta" --arg subs "${subs:-?}" --arg valb "$valb" --arg cross "${CROSS:-0}" \
    --argjson ia "$(echo "$itemsa" | jq -c '[.items[] | {o: .outcome, step: (.step.action // null), err: (.step.error // .result.reason // null)}]')" \
    '{engine:$e, kind:$k, budget_ms:($b|tonumber), flowA:$sta, flowA_ms:$ta, flowA_items:$ia, flowB:$stb, cross:$cross, submitsB:$subs, inputB:$valb, heardKeys:(.events), keysB:([.codeOf|keys[]]|join("")), mouseB:.mouse}'
api DELETE "/v1/sessions/$sid" "$T" > /dev/null
