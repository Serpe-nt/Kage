#!/usr/bin/env bash
# QA round four: the rhythm of a human `select`'s keys as the page hears it.
# A flow opens keys4.html with a select of OPTIONS options and chooses
# option N (enough time), then the page's key log is read: per key, its hold
# (down to up) and the gap from the previous key's up to its down.
#   bash sel4.sh STACK ENGINE [N] [OPTIONS]
# Prints one JSON line: how many keys the page heard, holds and gaps (min,
# median, max), how many gaps under 10 ms, and the select's value.
set -uo pipefail
S="${1:?stack}"
E="${2:?engine}"
N="${3:-20}"
O="${4:-40}"
# shellcheck disable=SC1091
source /scripts/qa/infra/browser/lib.sh "$S"
T="$(tok "qa4b-sel-$(uid)" fetch,jobs)"
id="sel$(uid)"
page keys4.html "/$id/keys"
a=$(jq -nc --arg u "http://qa.kage.test:8091/$id/keys?options=$O&$E" --arg v "option $N" \
    '[{action:"goto",url:$u},{action:"wait_for",selector:"#s"},{action:"select",selector:"#s",value:$v,timeout_ms:120000},{action:"wait_for",ms:1500}]')
jid=$(api POST /v1/jobs "$T" "$(jq -nc --arg e "$E" --argjson a "$a" '{type:"flow",level:3,max_retries:0,persona:{engines:[$e]},timeout_ms:180000,include:["dom"],actions:$a}')" | jq -r .id)
st=$(wait_job "$T" "$jid")
items=$(api GET "/v1/jobs/$jid/items" "$T")
ab=$(echo "$items" | jq -r '.items[-1].result.artifacts.dom_html // empty')
dom=""
[ -n "$ab" ] && dom=$(art "$T" "$ab")
mkdir -p "${OUT:-/tmp/sel4}" && printf %s "$dom" > "${OUT:-/tmp/sel4}/$E-$id.html"
printf %s "$dom" | node -e '
const h = require("fs").readFileSync(0, "utf8");
const m = h.match(/<pre id="log">([\s\S]*?)<\/pre>/);
const un = (s) => s.replace(/&quot;/g, "\"").replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&amp;/g, "&");
const ev = m && m[1].trim() ? JSON.parse(un(m[1])) : [];
const keys = ev.filter((e) => e[0] === "d" || e[0] === "u");
const holds = [], gaps = [];
let lastUp = null, downAt = {};
for (const e of keys) {
  if (e[0] === "d") { if (lastUp !== null) gaps.push(+(e[3] - lastUp).toFixed(1)); downAt[e[2]] = e[3]; }
  else { if (downAt[e[2]] !== undefined) holds.push(+(e[3] - downAt[e[2]]).toFixed(1)); lastUp = e[3]; }
}
const st = (a) => { const s = [...a].sort((x, y) => x - y); return a.length ? { n: a.length, min: s[0], med: s[Math.floor(s.length / 2)], max: s[s.length - 1] } : { n: 0 }; };
const by = {}; for (const e of keys) by[e[1] + "@" + e[5]] = (by[e[1] + "@" + e[5]] || 0) + 1;
const vs = (h.match(/<pre id="vs">([^<]*)</) || [])[1] || "";
console.log(JSON.stringify({ keys: by, holds: st(holds), gaps: st(gaps), gapsUnder10ms: gaps.filter((g) => g < 10).length, value: un(vs) }));
' | jq -c --arg e "$E" --arg st "$st" --arg n "$N" '{engine:$e, job:$st, option:($n|tonumber)} + .'
