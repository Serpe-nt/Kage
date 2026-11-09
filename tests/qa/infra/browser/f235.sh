#!/usr/bin/env bash
# F235 and around it: a flow whose click sends the tab to a page that never
# answers. Plan F235: Chromium ends `timeout`, Firefox ends `ok` with the page
# from before the click. This checks how far that goes, per engine:
#   last     goto A, click to the dead page (the plan's case)
#   extract  ... then an `extract`: whose page does it read?
#   waiturl  ... then `wait_for url` of the dead page's address
#   assert   ... then `assert` text of the dead page
#   e503     the click leads to a page that answers 503 (not dead)
#   same     the dead page is on the flow's own site
# and after each engine's flows, whether the dead site is paused (a batch of
# its /after page: asked at once, or held for the server-error pause).
# usage: bash f235.sh STACK [ENGINES] [VARIANTS]   (a stack that is up, `stack.sh fast` applied)
#   ENGINES default "chromium firefox", VARIANTS default all of the above.
# Prints one JSON line per flow and a summary per engine; items in OUT (default /tmp/f235).
set -uo pipefail
S="${1:?stack}"
ENGINES="${2:-chromium firefox}"
VARIANTS="${3:-last extract waiturl assert e503 same}"
# shellcheck disable=SC1091
source /scripts/qa/infra/browser/lib.sh "$S"
OUT="${OUT:-/tmp/f235}"
mkdir -p "$OUT"
T="$(tok "f235-$(uid)" fetch,jobs)"
FIRST=other.qa.test
LATER=qa.kage.test
html() { jq -cn --arg h "$1" --arg p "$2" --arg b "$3" --argjson d "${4:-0}" --argjson s "${5:-200}" '{host:$h,path:$p,status:$s,headers:{"content-type":"text/html"},body:$b,delay_ms:$d}'; }
page_with_link() { # HOST PATH TARGET
    setr "$(html "$1" "$2" "<html><head><title>before the click</title></head><body><main><h1>before</h1><p>Words a reader reads on this page, long enough to be content.</p><a id=go href='$3' style='display:block;width:300px;height:80px'>on</a></main></body></html>")"
}
run_flow() { # ENGINE VARIANT RUN
    local e="$1" v="$2" u="$3" target thost extra='[]' jid st items
    thost="$LATER"
    case "$v" in same) thost="$FIRST" ;; esac
    target="http://$thost:8091/$u/x-$e-$v"
    case "$v" in
        e503) setr "$(html "$thost" "/$u/x-$e-$v" "<html><head><title>after the click</title></head><body><main><h1>unavailable</h1></main></body></html>" 0 503)" ;;
        *) setr "$(html "$thost" "/$u/x-$e-$v" "<html><head><title>after the click</title></head><body><main><h1>after</h1><p>Words of the page after the click.</p></main></body></html>" 60000)" ;;
    esac
    page_with_link "$FIRST" "/$u/a-$e-$v" "$target"
    case "$v" in
        extract) extra='[{"action":"extract"}]' ;;
        waiturl) extra="[{\"action\":\"wait_for\",\"url\":\"**/x-$e-$v\",\"timeout_ms\":5000}]" ;;
        assert) extra='[{"action":"assert","text_contains":"Words of the page after the click","timeout_ms":5000}]' ;;
    esac
    body=$(jq -nc --arg e "$e" --arg a "http://$FIRST:8091/$u/a-$e-$v" --argjson x "$extra" \
        '{type:"flow",level:2,max_retries:0,persona:{engines:[$e]},include:["extract"],actions:([{action:"goto",url:$a},{action:"click",selector:"#go",timeout_ms:5000}] + $x)}')
    local t0; t0=$(date +%s%3N)
    jid=$(api POST /v1/jobs "$T" "$body" | jq -r .id)
    st=$(wait_job "$T" "$jid")
    items=$(api GET "/v1/jobs/$jid/items" "$T")
    echo "$items" > "$OUT/$e-$v.items.json"
    echo "$items" | jq -c --arg e "$e" --arg v "$v" --arg st "$st" --argjson ms "$(($(date +%s%3N) - t0))" '
        {engine:$e, variant:$v, job:$st, ms:$ms,
         steps:[.items[] | {a:.step.action, ok:.step.ok, err:(.step.error // .error), ms:.step.duration_ms}],
         result:(.items[-1].result | {outcome, reason, status, final_url, title,
                 text:((.extract.text // .extract.markdown // .content // "") | tostring | .[0:80])})}'
}
for e in $ENGINES; do
    u="f235-$e-$(uid)"
    for v in $VARIANTS; do run_flow "$e" "$v" "$u"; done
    # Is the dead site paused now? A batch of a page that answers at once, on each site.
    for h in "$FIRST" "$LATER"; do setr "$(html "$h" "/$u/after" "<html><body><main><h1>after</h1><p>Words a reader reads.</p></main></body></html>")"; done
    ended=$(date +%s)
    for h in "$FIRST" "$LATER"; do
        api POST /v1/jobs "$T" "{\"type\":\"batch\",\"urls\":[\"http://$h:8091/$u/after\"],\"defaults\":{\"level\":2,\"max_retries\":0,\"persona\":{\"engines\":[\"$e\"]}}}" > /dev/null
    done
    timeout 50 tail -f /dev/null
    at() { rlog "/$u/after" | jq -r --arg h "$1" '[.[] | select(.host == $h) | .at][0] // ""'; }
    f="$(at "$FIRST")"
    l="$(at "$LATER")"
    fd=$(($(date -d "$f" +%s 2> /dev/null || echo 99999999999) - ended))
    ld=$(($(date -d "$l" +%s 2> /dev/null || echo 99999999999) - ended))
    echo "{\"engine\":\"$e\",\"after\":{\"first_site_s\":$fd,\"later_site_s\":$ld}}"
    rlog "/$u/" | jq -c '[.[] | {at, host, url}]' > "$OUT/$e.site-log.json"
done
