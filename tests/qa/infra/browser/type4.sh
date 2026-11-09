#!/usr/bin/env bash
# QA round four: types TEXT_I into the input and TEXT_T into the textarea of
# keys4.html in a level 3 flow of one engine, then reads the page's key log
# and prints keys4-analyze.mjs's report (one JSON line).
#   bash type4.sh STACK ENGINE TEXT_I [TEXT_T]
# Environment: HUMAN (true), TYPOS (false), EXTRA (steps after the typing,
# JSON, default []), SESSION (a session id to run in), TYPE_TIMEOUT (the
# type steps' timeout_ms, default 240000), OPTIONS (options of the select),
# OUT (where the job's items and the document go, default /tmp/type4-ENGINE).
set -uo pipefail
STACK="${1:?stack}"
E="${2:?engine}"
TI="${3:-}"
TT="${4:-}"
# shellcheck disable=SC1091
source /scripts/qa/infra/browser/lib.sh "$STACK"
HUMAN="${HUMAN:-true}"
TYPOS="${TYPOS:-false}"
EXTRA="${EXTRA:-[]}"
OUT="${OUT:-/tmp/type4-$E}"
mkdir -p "$(dirname "$OUT")"
T="${QA_TOKEN:-$(tok "qa4b-type-$(uid)" fetch,jobs,sessions)}"
id="ty$(uid)"
page keys4.html "/$id/keys"
url="http://qa.kage.test:8091/$id/keys?options=${OPTIONS:-0}&$E"
steps=$(jq -nc --arg u "$url" --arg ti "$TI" --arg tt "$TT" --argjson h "$HUMAN" --argjson x "$EXTRA" --argjson to "${TYPE_TIMEOUT:-240000}" '
  [{action:"goto",url:$u},{action:"wait_for",selector:"#i"}]
  + (if $ti != "" then [{action:"type",selector:"#i",text:$ti,human:$h,timeout_ms:$to}] else [] end)
  + (if $tt != "" then [{action:"type",selector:"#t",text:$tt,human:$h,timeout_ms:$to}] else [] end)
  + $x + [{action:"wait_for",ms:300}]')
body=$(jq -nc --arg e "$E" --argjson s "$steps" --argjson ty "$TYPOS" --arg sid "${SESSION:-}" '
  {type:"flow",level:3,max_retries:0,behavior:{dwell_ms:500,typos:$ty},timeout_ms:290000,include:["dom"],actions:$s}
  + (if $sid != "" then {session_id:$sid} else {persona:{engines:[$e]}} end)')
j=$(api POST /v1/jobs "$T" "$body")
jid=$(echo "$j" | jq -r .id)
if [ "$jid" = null ] || [ -z "$jid" ]; then
    echo "{\"refused\":$(echo "$j" | jq -c . 2> /dev/null || echo '"?"')}"
    exit 1
fi
st=$(wait_job "$T" "$jid")
items=$(api GET "/v1/jobs/$jid/items" "$T")
echo "$items" > "$OUT.items.json"
a=$(echo "$items" | jq -r '.items[-1].result.artifacts.dom_html // empty')
dom=""
[ -n "$a" ] && dom=$(art "$T" "$a")
printf %s "$dom" > "$OUT.dom.html"
jq -n --rawfile d "$OUT.dom.html" '{dom_html:$d}' > "$OUT.dom.json"
steps_seen=$(echo "$items" | jq -c '[.items[] | {o: .outcome, step: (.step.action // null), ok: (.step.ok // null), err: (.step.error // .result.reason // null)}]')
node /scripts/qa/infra/browser/keys4-analyze.mjs "$TI" "$TT" < "$OUT.dom.json" |
    jq -c --arg st "$st" --arg e "$E" --argjson steps "$steps_seen" '{engine:$e, job:$st, steps:$steps} + .'
