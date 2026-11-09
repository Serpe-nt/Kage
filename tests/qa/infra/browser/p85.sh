#!/usr/bin/env bash
# QA round four, after #85: probes next to each road #85 changed, both engines
# unless said. One JSON line per case; the evidence in OUT (default /tmp/p85).
#   bash p85.sh STACK [PARTS]   (a stack that is up, `stack.sh fast` applied)
# PARTS (default all): hdr h304 cache flowbound hidden c304 dl
#   hdr        repeated header lines (link, x-multi, vary, set-cookie) at level 2,
#              and again after a 304 (Firefox's kept heads): one entry per line,
#              no line feed in a value (browser-01), order as sent (browser-04)
#   h304       a 304 that names other headers (content-type, content-language,
#              cache-control, x-after): the kept page's type and extract stay
#              (browser-03), what else the result takes from the 304
#   cache      (P85_ONLY: the variants to run) a page revalidated in a session (200 then 304) with include raw:
#              raw_body equals the first body for identity, gzip, br, deflate
#              (zlib and raw), zstd, chunked, 600 KB, a Vary page, after a
#              redirect, and cut at a request's max_body_bytes (browser-05)
#   flowbound  a flow's max_body_bytes: its final page and an extract step
#              (extract-04)
#   hidden     links a style sheet hides, with { } ` %5E, ^ and | in the query,
#              a space, a quote: none in extract.links (extract-06)
#   c304       a 304 to an address nothing was kept for: typed, a flow's goto,
#              behind a redirect (class-02)
#   dl         a download with include raw: typed, a flow's click, a batch item;
#              no bytes in the result, its artifacts or the job's items (crawl-04)
set -uo pipefail
S="${1:?stack}"
PARTS="${2:-hdr h304 cache flowbound hidden c304 dl}"
# shellcheck disable=SC1091
source /scripts/qa/infra/browser/lib.sh "$S"
OUT="${OUT:-/tmp/p85}"
mkdir -p "$OUT"
T="$(tok "qa4b-p85-$(uid)" fetch,jobs,sessions)"
R="p85$(uid)"
BASE="http://qa.kage.test:8091/$R"
HTML='<html><head><title>A kept page</title></head><body><main><h1>A kept page</h1><p>Words a reader reads on this page of the site, a paragraph long enough to be content and not blank.</p></main></body></html>'
session() { # ID ENGINE
    api POST /v1/sessions "$T" "{\"id\":\"$1\",\"domain\":\"qa.kage.test\",\"persona\":{\"engines\":[\"$2\"]}}" > /dev/null
}
unsession() { api DELETE "/v1/sessions/$1" "$T" > /dev/null; }
fetch() { # JSON -> result (also kept in OUT/last.json)
    fx "$T" "$1" | tee "$OUT/last.json"
}

part_hdr() {
    for e in chromium firefox; do
        p="/$R/hdr-$e"
        setr "$(jq -nc --arg p "$p" --arg b "$HTML" '{path:$p,times:[
            {status:200,headers:{"content-type":"text/html; charset=utf-8",etag:"\"h1\"","cache-control":"no-cache",
              "x-first":"1",link:["</a.css>; rel=preload; as=style","</b.js>; rel=preload; as=script"],
              "x-multi":["one","two","three"],vary:["accept-encoding","accept-language"],
              "set-cookie":["qa_a=1; Path=/","qa_b=2; Path=/"],"x-last":"z"},body:$b},
            {status:304,headers:{etag:"\"h1\"","x-multi":["four","five"]},body:""}]}')"
        sid="$R-hdr-$e"
        session "$sid" "$e"
        for n in 1 2; do
            r=$(fetch "{\"url\":\"http://qa.kage.test:8091$p\",\"level\":2,\"session_id\":\"$sid\",\"max_retries\":0,\"include\":[\"headers\"]}")
            echo "$r" | jq -c --arg e "$e" --argjson n "$n" '{part:"hdr",engine:$e,visit:$n,outcome,status,
                lf:[.headers[]? | select(.value|test("\n")) | .name],
                multi:[.headers[]? | select(.name|ascii_downcase=="x-multi") | .value],
                link:[.headers[]? | select(.name|ascii_downcase=="link") | .value] | length,
                vary:[.headers[]? | select(.name|ascii_downcase=="vary") | .value],
                cookies:[.headers[]? | select(.name|ascii_downcase=="set-cookie")] | length,
                order:[.headers[]? | .name | ascii_downcase | select(. == "x-first" or . == "link" or . == "x-multi" or . == "vary" or . == "x-last")]}'
        done
        unsession "$sid"
    done
}

part_h304() {
    for e in chromium firefox; do
        p="/$R/h304-$e"
        setr "$(jq -nc --arg p "$p" --arg b "$HTML" '{path:$p,times:[
            {status:200,headers:{"content-type":"text/html; charset=utf-8",etag:"\"t1\"","cache-control":"no-cache","content-language":"en"},body:$b},
            {status:304,headers:{etag:"\"t1\"","content-type":"text/plain","content-language":"de","cache-control":"no-cache, private","x-after":"1"},body:""}]}')"
        sid="$R-h304-$e"
        session "$sid" "$e"
        for n in 1 2 3; do
            r=$(fetch "{\"url\":\"http://qa.kage.test:8091$p\",\"level\":2,\"session_id\":\"$sid\",\"max_retries\":0,\"include\":[\"headers\",\"extract\"]}")
            echo "$r" | jq -c --arg e "$e" --argjson n "$n" '{part:"h304",engine:$e,visit:$n,outcome,status,
                ctype:[.headers[]? | select(.name|ascii_downcase=="content-type") | .value],
                clang:[.headers[]? | select(.name|ascii_downcase=="content-language") | .value],
                cc:[.headers[]? | select(.name|ascii_downcase=="cache-control") | .value],
                after:[.headers[]? | select(.name|ascii_downcase=="x-after") | .value],
                extract_title:(.extract.title // null)}'
        done
        unsession "$sid"
    done
}

part_cache() {
    # The bodies, encoded by node; one line per variant: NAME ENCODING BASE64 [EXTRA_HEADERS_JSON]
    node -e '
const zlib = require("node:zlib");
const html = process.argv[1];
const big = html.replace("</main>", "<p>" + "Many words of a long page that a reader reads. ".repeat(13000) + "</p></main>");
const out = [];
const add = (name, enc, buf, extra = {}, text = html) => out.push({ name, enc, b64: buf.toString("base64"), extra, want: text });
add("identity", null, Buffer.from(html));
add("gzip", "gzip", zlib.gzipSync(html));
add("br", "br", zlib.brotliCompressSync(html));
add("deflate-zlib", "deflate", zlib.deflateSync(html));
add("deflate-raw", "deflate", zlib.deflateRawSync(html));
if (zlib.zstdCompressSync) add("zstd", "zstd", zlib.zstdCompressSync(html));
add("chunked", "gzip", zlib.gzipSync(html), { "transfer-encoding": "chunked" });
add("big-gzip", "gzip", zlib.gzipSync(big), {}, big);
add("vary", "gzip", zlib.gzipSync(html), { vary: "accept-encoding" });
const only = (process.env.P85_ONLY || "").split(" ").filter(Boolean);
for (const o of out) if (only.length === 0 || only.includes(o.name)) console.log(JSON.stringify(o));
' "$HTML" > "$OUT/bodies.jsonl"
    for e in firefox chromium; do
        while read -r line <&3; do
            name=$(echo "$line" | jq -r .name)
            p="/$R/c-$e-$name"
            echo "$line" | jq -c --arg p "$p" '{path:$p,times:[
                {base64:true,status:200,headers:({"content-type":"text/html; charset=utf-8",etag:"\"c1\"","cache-control":"no-cache"}
                   + (if .enc then {"content-encoding":.enc} else {} end) + .extra),body:.b64},
                {status:304,headers:{etag:"\"c1\""},body:""}]}' > "$OUT/route.json"
            setr "$(cat "$OUT/route.json")"
            sid="$R-c-$e-$name"
            session "$sid" "$e"
            fetch "{\"url\":\"http://qa.kage.test:8091$p\",\"level\":2,\"session_id\":\"$sid\",\"max_retries\":0,\"include\":[\"raw\"]}" > /dev/null
            r=$(fetch "{\"url\":\"http://qa.kage.test:8091$p\",\"level\":2,\"session_id\":\"$sid\",\"max_retries\":0,\"include\":[\"raw\"]}")
            unsession "$sid"
            got304=$(rlog "$p" < /dev/null | jq '[.[] | select(.headers["if-none-match"] != null)] | length')
            echo "$line" | jq -j .want > "$OUT/want.txt"
            echo "$r" | jq -c --arg e "$e" --arg n "$name" --argjson g "$got304" --rawfile want "$OUT/want.txt" '{part:"cache",engine:$e,variant:$n,
                revalidated:($g > 0),outcome,status,truncated,
                raw_len:(.raw_body|if .==null then null else length end), want_len:($want|length), same:(.raw_body == $want),
                artifact:(.artifacts.raw_body != null)}'
        done 3< "$OUT/bodies.jsonl"
        # A body cut at the request's max_body_bytes on the revisit.
        p="/$R/c-$e-cut"
        setr "$(head -1 "$OUT/bodies.jsonl" | jq -c --arg p "$p" '{path:$p,times:[
            {base64:true,status:200,headers:{"content-type":"text/html; charset=utf-8",etag:"\"c1\"","cache-control":"no-cache"},body:.b64},
            {status:304,headers:{etag:"\"c1\""},body:""}]}')"
        sid="$R-c-$e-cut"
        session "$sid" "$e"
        fetch "{\"url\":\"http://qa.kage.test:8091$p\",\"level\":2,\"session_id\":\"$sid\",\"max_retries\":0,\"include\":[\"raw\"]}" > /dev/null
        r=$(fetch "{\"url\":\"http://qa.kage.test:8091$p\",\"level\":2,\"session_id\":\"$sid\",\"max_retries\":0,\"max_body_bytes\":100,\"include\":[\"raw\"]}")
        unsession "$sid"
        echo "$r" | jq -c --arg e "$e" '{part:"cache",engine:$e,variant:"cut-100",outcome,status,truncated,raw_len:(.raw_body|if .==null then null else length end)}'
        # Behind a redirect: /r answers 302 to the page each time; the page 200 then 304.
        p="/$R/c-$e-target"
        setr "$(head -1 "$OUT/bodies.jsonl" | jq -c --arg p "$p" '{path:$p,times:[
            {base64:true,status:200,headers:{"content-type":"text/html; charset=utf-8",etag:"\"c1\"","cache-control":"no-cache"},body:.b64},
            {status:304,headers:{etag:"\"c1\""},body:""}]}')"
        setr "$(jq -nc --arg p "/$R/c-$e-r" --arg l "$BASE/c-$e-target" '{path:$p,status:302,headers:{location:$l},body:""}')"
        sid="$R-c-$e-r"
        session "$sid" "$e"
        fetch "{\"url\":\"$BASE/c-$e-r\",\"level\":2,\"session_id\":\"$sid\",\"max_retries\":0,\"include\":[\"raw\"]}" > /dev/null
        r=$(fetch "{\"url\":\"$BASE/c-$e-r\",\"level\":2,\"session_id\":\"$sid\",\"max_retries\":0,\"include\":[\"raw\"]}")
        unsession "$sid"
        echo "$r" | jq -c --arg e "$e" --arg want "$HTML" '{part:"cache",engine:$e,variant:"after-redirect",outcome,status,final_url,same:(.raw_body == $want),raw_len:(.raw_body|if .==null then null else length end)}'
    done
}

part_flowbound() {
    big='<html><head><title>Long</title></head><body><main><h1>Long</h1><p>'"$(printf 'Words of a long page that a reader reads. %.0s' $(seq 1 400))"'</p></main></body></html>'
    setr "$(jq -nc --arg p "/$R/fb" --arg b "$big" '{path:$p,status:200,headers:{"content-type":"text/html; charset=utf-8"},body:$b}')"
    for e in chromium firefox; do
        body=$(jq -nc --arg e "$e" --arg u "$BASE/fb" '{type:"flow",level:2,max_retries:0,max_body_bytes:3000,persona:{engines:[$e]},include:["raw","extract"],
            actions:[{action:"goto",url:$u},{action:"extract"},{action:"wait_for",ms:300}]}')
        jid=$(api POST /v1/jobs "$T" "$body" | jq -r .id)
        st=$(wait_job "$T" "$jid")
        api GET "/v1/jobs/$jid/items" "$T" > "$OUT/fb-$e.json"
        jq -c --arg e "$e" --arg st "$st" --argjson big "${#big}" '{part:"flowbound",engine:$e,job:$st,page_bytes:$big,
            final:(.items[-1].result | {outcome,truncated,raw_len:(.raw_body|if .==null then null else length end)}),
            steps:[.items[] | select(.step.action == "extract") | {truncated:(.step.capture.truncated // .result.truncated // null),
                raw_len:((.step.capture.raw_body // .result.raw_body) | if .==null then null else length end)}]}' "$OUT/fb-$e.json"
    done
}

part_hidden() {
    page='<html><head><title>Links</title><style>.gone{display:none}</style></head><body><main><h1>Links</h1><p>Words a reader reads on this page, a paragraph long enough to be content.</p>
<a href="/'"$R"'/seen">seen</a>
<a class="gone" href="/'"$R"'/h/a{b}">brace</a>
<a class="gone" href="/'"$R"'/h/a`b">backtick</a>
<a class="gone" href="/'"$R"'/h/a%5Eb">pct5e</a>
<a class="gone" href="/'"$R"'/h/q?x=a^b">caret-query</a>
<a class="gone" href="/'"$R"'/h/q2?x=a|b">pipe-query</a>
<a class="gone" href="/'"$R"'/h/a b">space</a>
<a class="gone" href="/'"$R"'/h/a&quot;b">quote</a>
<a class="gone" href="/'"$R"'/h/a^b#f|g">caret-frag</a>
<a class="gone" href="/'"$R"'/h/a%7Cb">pct7c</a>
</main></body></html>'
    setr "$(jq -nc --arg p "/$R/links" --arg b "$page" '{path:$p,status:200,headers:{"content-type":"text/html; charset=utf-8"},body:$b}')"
    for e in chromium firefox; do
        r=$(fetch "{\"url\":\"$BASE/links\",\"level\":2,\"max_retries\":0,\"persona\":{\"engines\":[\"$e\"]},\"include\":[\"extract\"]}")
        echo "$r" | jq -c --arg e "$e" '{part:"hidden",engine:$e,outcome,links:[.extract.links[]?.url | sub("^.*/p85[0-9]+"; "")]}'
    done
}

part_c304() {
    for e in chromium firefox; do
        setr "$(jq -nc --arg p "/$R/n-$e" '{path:$p,status:304,headers:{etag:"\"n1\""},body:""}')"
        r=$(fetch "{\"url\":\"$BASE/n-$e\",\"level\":2,\"max_retries\":0,\"persona\":{\"engines\":[\"$e\"]}}")
        echo "$r" | jq -c --arg e "$e" '{part:"c304",engine:$e,road:"typed",outcome,reason,status,attempts:(.attempts|length)}'
        setr "$(jq -nc --arg p "/$R/nr-$e" --arg l "$BASE/n-$e" '{path:$p,status:302,headers:{location:$l},body:""}')"
        r=$(fetch "{\"url\":\"$BASE/nr-$e\",\"level\":2,\"max_retries\":0,\"persona\":{\"engines\":[\"$e\"]}}")
        echo "$r" | jq -c --arg e "$e" '{part:"c304",engine:$e,road:"redirect",outcome,reason,status,final_url,attempts:(.attempts|length)}'
        body=$(jq -nc --arg e "$e" --arg u "$BASE/n-$e" '{type:"flow",level:2,max_retries:0,persona:{engines:[$e]},actions:[{action:"goto",url:$u}]}')
        jid=$(api POST /v1/jobs "$T" "$body" | jq -r .id)
        st=$(wait_job "$T" "$jid")
        api GET "/v1/jobs/$jid/items" "$T" | jq -c --arg e "$e" --arg st "$st" '{part:"c304",engine:$e,road:"flow goto",job:$st,
            steps:[.items[] | {a:.step.action, ok:.step.ok, err:(.step.error // null)}], result:(.items[-1].result | {outcome,reason,status})}'
    done
}

part_dl() {
    marker="QA-DOWNLOAD-MARKER-$R"
bytes=$(printf "%s " $(seq 1 200 | sed "s/.*/$marker/") | base64 -w0)
    setr "$(jq -nc --arg p "/$R/file.bin" --arg b "$bytes" '{path:$p,status:200,base64:true,headers:{"content-type":"application/octet-stream","content-disposition":"attachment; filename=\"file.bin\""},body:$b}')"
    setr "$(jq -nc --arg p "/$R/dlpage" --arg r "$R" '{path:$p,status:200,headers:{"content-type":"text/html; charset=utf-8"},body:("<html><body><main><h1>Files</h1><p>Words a reader reads on this page, long enough.</p><a id=\"f\" href=\"/" + $r + "/file.bin\" style=\"display:block;width:300px;height:60px\">the file</a></main></body></html>")}')"
    for e in chromium firefox; do
        r=$(fetch "{\"url\":\"$BASE/file.bin\",\"level\":2,\"max_retries\":0,\"persona\":{\"engines\":[\"$e\"]},\"include\":[\"raw\"]}")
        echo "$r" | jq -c --arg e "$e" --arg b "$marker" '{part:"dl",engine:$e,road:"typed",outcome,reason,status,raw_body:(.raw_body != null),artifacts:(.artifacts|keys? // []),bytes_in_result:(tostring | contains($b))}'
        body=$(jq -nc --arg e "$e" --arg u "$BASE/dlpage" '{type:"flow",level:2,max_retries:0,persona:{engines:[$e]},include:["raw"],actions:[{action:"goto",url:$u},{action:"click",selector:"#f"},{action:"wait_for",ms:1500}]}')
        jid=$(api POST /v1/jobs "$T" "$body" | jq -r .id)
        st=$(wait_job "$T" "$jid")
        items=$(api GET "/v1/jobs/$jid/items" "$T")
        echo "$items" | jq -c --arg e "$e" --arg st "$st" --arg b "$marker" '{part:"dl",engine:$e,road:"flow click",job:$st,
            result:(.items[-1].result | {outcome,reason,status,final_url,artifacts:(.artifacts|keys? // [])}), bytes_in_items:(tostring | contains($b))}'
        body=$(jq -nc --arg e "$e" --arg u "$BASE/file.bin" '{type:"batch",urls:[$u],defaults:{level:2,max_retries:0,include:["raw"],persona:{engines:[$e]}}}')
        jid=$(api POST /v1/jobs "$T" "$body" | jq -r .id)
        st=$(wait_job "$T" "$jid")
        items=$(api GET "/v1/jobs/$jid/items" "$T")
        echo "$items" | jq -c --arg e "$e" --arg st "$st" --arg b "$marker" '{part:"dl",engine:$e,road:"batch",job:$st,
            result:(.items[-1].result | {outcome,reason,status,raw_body:(.raw_body != null),artifacts:(.artifacts|keys? // [])}), bytes_in_items:(tostring | contains($b))}'
    done
}

for part in $PARTS; do
    "part_$part" 2>&1 | tee -a "$OUT/p85.jsonl"
done
