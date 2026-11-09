# QA rounds three and four, the browsers: helpers. Source with the stack name:
#   source /scripts/qa/infra/browser/lib.sh STACK
#   tok NAME SCOPES      a fresh key's token (never printed by the helpers)
#   setr JSON            program the responder
#   rlog FILTER          the responder's log entries whose url contains FILTER (JSON array)
#   fx TOKEN JSON        POST /v1/fetch, prints the body only
#   api METHOD PATH TOKEN [JSON]   any call, prints the body only
#   wait_job TOKEN JOB   waits for a job to end (at most 5 min), prints its state
#   art TOKEN ID         an artifact's body
#   sq SQL               psql in the stack's Postgres
#   uid                  a short unique id
#   page FILE PATH       serve a file of this directory at PATH on qa.kage.test:8091
S="${1:?stack name}"
H=/scripts/qa/stack.sh
B=/scripts/qa/infra/browser
# shellcheck disable=SC1090
source "/target/qa/runs/${S}/env.sh"
U="http://127.0.0.1:${KAGE_PUBLISH_PORT}"
tok() { bash $H key "$S" "$1" "$2"; }
setr() { bash $H responder "$S" "$1" > /dev/null; }
rlog() {
    bash $H dc "$S" exec -T world node -e "fetch('http://qa.kage.test:8091/__log').then(r=>r.json()).then(l=>console.log(JSON.stringify(l.filter(e=>e.url.includes(process.argv[1])))))" "$1"
}
fx() {
    curl -s --max-time "${QA_MAX_TIME:-300}" -H "Authorization: Bearer $1" -H 'Content-Type: application/json' -d "$2" "$U/v1/fetch"
}
api() {
    local a=(-s --max-time 300 -X "$1" -H "Authorization: Bearer $3")
    [ -n "${4:-}" ] && a+=(-H 'Content-Type: application/json' -d "$4")
    curl "${a[@]}" "$U$2"
}
wait_job() {
    local st=""
    for _ in $(seq 1 300); do
        st=$(api GET "/v1/jobs/$2" "$1" | jq -r .state)
        case $st in succeeded | partial | failed | cancelled) break ;; esac
        timeout 1 tail -f /dev/null
    done
    echo "$st"
}
art() { curl -s -H "Authorization: Bearer $1" "$U/v1/artifacts/$2"; }
sq() { bash $H sql "$S" "$1"; }
uid() { date +%s%N | tail -c 8; }
page() {
    setr "$(jq -nc --arg p "$2" --rawfile b "$B/$1" '{path:$p,status:200,headers:{"content-type":"text/html; charset=utf-8"},body:$b}')"
}
