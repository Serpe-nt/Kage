# Helpers of the flows agent. Source with STACK set.
H=/scripts/qa/stack.sh
: "${STACK:?STACK}"
BASE="$(bash $H url "$STACK")"
ADMIN="$(bash $H admin "$STACK")"
STF="/tmp/flows-st-$$"
# api METHOD PATH TOKEN [JSON] -> body on stdout; status: st
api() {
    local m="$1" p="$2" t="$3" d="${4:-}" out
    out="$(mktemp)"
    if [ -n "$d" ]; then
        curl -s -o "$out" -w '%{http_code}' --max-time "${MAXT:-300}" -X "$m" -H "Authorization: Bearer $t" -H 'Content-Type: application/json' --data "$d" "$BASE$p" > "$STF"
    else
        curl -s -o "$out" -w '%{http_code}' --max-time "${MAXT:-300}" -X "$m" -H "Authorization: Bearer $t" "$BASE$p" > "$STF"
    fi
    cat "$out"; rm -f "$out"
}
st() { cat "$STF"; }
newkey() { bash $H key "$STACK" "$1" "${2:-fetch,jobs}" "${3:-}"; }
resp() { bash $H responder "$STACK" "$1" > /dev/null; }
rlog() { bash $H dc "$STACK" exec -T world node -e "fetch('http://qa.kage.test:8091/__log').then(r=>r.text()).then(console.log)"; }
sql() { bash $H sql "$STACK" "$1"; }
# wait_job TOKEN ID [tries] -> final job json
wait_job() {
    local t="$1" id="$2" n="${3:-150}" j s
    for _ in $(seq 1 "$n"); do
        j="$(api GET "/v1/jobs/$id" "$t")"
        s="$(printf '%s' "$j" | jq -r .state)"
        case "$s" in succeeded|partial|failed|cancelled) printf '%s' "$j"; return 0;; esac
        sleep 2
    done
    printf '%s' "$j"
}
items() { api GET "/v1/jobs/$2/items?limit=200" "$1"; }
# flow KEY BODY-JSON -> prints submit status and the items summary; JOB holds the id in /tmp
flow_run() {
    local j jid; j="$(api POST /v1/jobs "$1" "$2")"
    if [ "$(st)" != 201 ]; then echo "submit $(st) $(printf '%s' "$j" | jq -c '{code,detail,step}')"; return; fi
    jid="$(printf '%s' "$j" | jq -r .id)"; echo "$jid" > /tmp/flows-lastjob-$$
    local fj; fj="$(wait_job "$1" "$jid")"
    echo "job $(printf '%s' "$fj" | jq -c '{state,error}')"
    items "$1" "$jid" | jq -c '[.items[] | {a:.step.action, ok:.step.ok, err:(.step.error // .error), o:.outcome, r:.result.reason, url:.result.final_url, p:.result.persona.id}]' | cut -c1-1200
}
