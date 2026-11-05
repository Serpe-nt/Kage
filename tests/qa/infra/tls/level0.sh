#!/usr/bin/env bash
# Level 0 over https, end to end (F191, F170, F171; F90 observed): a stack
# of its own with the tls world and `http.ca_file` naming the world's
# private CA (compose.tls-level0.yaml), then
#
#   1. a page under the private CA is read at level 0 (`ok`, 200, its body);
#   2. a certificate the CA did not sign (unknown CA, self-signed) is the
#      site's answer: `connection_failed`, never `proxy_failed`, the proxies
#      stay healthy (tls-01);
#   3. a site's own 407 inside TLS is the site's answer: `ok` with 407;
#   4. a robots.txt over https with a NUL in a rule is read and kept once,
#      its rule enforced at level 0 (http-01's file, over https);
#   5. a robots.txt behind a certificate the client refuses is unreachable
#      for `robots.error_ttl` (an hour), not asked for again a minute later;
#   6. observed, not judged (F90 is open): a robots.txt that answers 304
#      once its kept answer expired.
#
#   bash /scripts/qa/infra/tls/level0.sh [STACK]     (default l0tls; brings it up and down)
#
# Ends with ALL-PASSED, or FAILED and the number of failed checks.
set -uo pipefail
S="${1:-l0tls}"
H=/scripts/qa/stack.sh
T=/scripts/qa/infra/tls
bash "$T/mkcerts.sh" > /dev/null
bash "$H" up "$S" "--overlay=$T/compose.tls.yaml" "--overlay=$T/compose.tls-level0.yaml" > "/tmp/$S-up.log" 2>&1 || {
    tail -n 20 "/tmp/$S-up.log"
    echo FAILED setup
    exit 1
}
bash "$H" fast "$S" > /dev/null
bash "$H" add-domains "$S" < "$T/level0-domains.yaml" > /dev/null
# shellcheck source=/dev/null
source "$T/lib.sh" "$S"
KEY="$(bash "$H" key "$S" "level0-tls-$RANDOM" fetch,jobs)"
failed=0
check() { # name condition-exit-status
    if [ "$2" = 0 ]; then echo "PASS $1"; else
        echo "FAIL $1"
        failed=$((failed + 1))
    fi
}
field() { echo "$1" | jq -r "$2"; }
tls_reset > /dev/null

echo "== 1. a page under the private CA, at level 0"
tls_set '{"host":"www.tlsf1.test","path":"/l0ok","status":200,"headers":{"content-type":"text/html"},"body":"<html><head><title>level zero over https</title></head><body><p>read at level 0</p></body></html>"}' > /dev/null
r="$(fetch "$KEY" '{"url":"https://www.tlsf1.test/l0ok","level":0,"max_retries":0,"include":["raw","extract"]}')"
echo "$r" | jq -c '{outcome,status,reason,title:.extract.title,exit:[.attempts[]|.exit]}'
[ "$(field "$r" .outcome)" = ok ] && [ "$(field "$r" .status)" = 200 ] && field "$r" .raw_body | grep -q 'level zero over https'
check "level 0 reads https://www.tlsf1.test/l0ok under the private CA" $?
tls_log | jq -c '.[]|select(.url=="/l0ok")|{scheme,sni,tls,ua:.headers["user-agent"]}'

echo "== 2. certificates the client refuses are the site's answer"
for host in unknownca.kage.test selfsigned.kage.test; do
    r="$(fetch "$KEY" "{\"url\":\"https://${host}/l0bad\",\"level\":0,\"max_retries\":0}")"
    echo "$host: $(echo "$r" | jq -c '{outcome,reason,attempts:[.attempts[]|.reason]}')"
    [ "$(field "$r" .outcome)" = provider_error ] && [ "$(field "$r" .reason)" = connection_failed ]
    check "level 0 https://${host}/ is connection_failed" $?
done
tls_handshakes | jq -c '.[]|{sni,error}' | sort | uniq -c
bash "$H" dc "$S" logs --no-color kage 2> /dev/null | grep '"an http attempt failed"' | sed 's/^[^|]*| //' | jq -rc '.fields.error' | sort | uniq -c
states="$(bash "$H" cli "$S" proxy list --json 2> /dev/null | jq -c '[.proxies[]|.state]')"
echo "proxies: $states"
[ "$(echo "$states" | jq '[.[]|select(.!="healthy")]|length')" = 0 ]
check "every proxy stays healthy" $?

echo "== 3. a site's own 407 inside TLS"
tls_set '{"host":"www.tlsf2.test","path":"/p407","status":407,"headers":{"proxy-authenticate":"Basic realm=\"site\"","content-type":"text/html"},"body":"<html><body>the site asks</body></html>"}' > /dev/null
r="$(fetch "$KEY" '{"url":"https://www.tlsf2.test/p407","level":0,"max_retries":0}')"
echo "$r" | jq -c '{outcome,status,reason}'
[ "$(field "$r" .outcome)" = ok ] && [ "$(field "$r" .status)" = 407 ]
check "level 0 https 407 is ok with the status" $?

echo "== 4. a robots.txt over https with a NUL in a rule"
tls_set "$(jq -nc '{host:"www.tlsf3.test",path:"/robots.txt",status:200,headers:{"content-type":"text/plain"},body:"User-agent: *\nDisallow: /private\u0000\nDisallow: /nope\n"}')" > /dev/null
tls_set '{"host":"www.tlsf3.test","path":"/public","status":200,"headers":{"content-type":"text/html"},"body":"<html><body>public</body></html>"}' > /dev/null
for i in 1 2 3; do
    r="$(fetch "$KEY" '{"url":"https://www.tlsf3.test/private/x","level":0,"max_retries":0}')"
    echo "private $i: $(echo "$r" | jq -c '{outcome,reason,robots}')"
done
[ "$(field "$r" .outcome)" = robots_denied ] && [ "$(field "$r" .robots.allowed)" = false ]
check "the rule of a robots.txt with a NUL is enforced" $?
r="$(fetch "$KEY" '{"url":"https://www.tlsf3.test/public","level":0,"max_retries":0}')"
[ "$(field "$r" .outcome)" = ok ]
check "an allowed page of that site is read" $?
asked="$(tls_log | jq '[.[]|select(.sni=="www.tlsf3.test" and .url=="/robots.txt")]|length')"
row="$(bash "$H" sql "$S" "SELECT status, unreachable FROM robots_cache WHERE origin = 'https://www.tlsf3.test'")"
echo "robots.txt asked for ${asked} time(s); robots_cache: ${row}"
[ "$asked" = 1 ] && [ "$row" = "200|f" ]
check "the file was asked for once and kept" $?

echo "== 5. a robots.txt behind a refused certificate"
r="$(fetch "$KEY" '{"url":"https://expired.kage.test/page","level":0,"max_retries":0}')"
echo "$r" | jq -c '{outcome,reason,retry_at}'
row="$(bash "$H" sql "$S" "SELECT unreachable, round(extract(epoch FROM expires_at - fetched_at)) FROM robots_cache WHERE origin = 'https://expired.kage.test'")"
echo "robots_cache: ${row}"
[ "$(field "$r" .reason)" = robots_unavailable ] && [ "${row%%|*}" = t ] && [ "${row##*|}" -ge 3000 ]
check "a refused certificate keeps the file unreachable for the error ttl" $?
r="$(fetch "$KEY" '{"url":"https://expired.kage.test/other","level":0,"max_retries":0}')"
hs="$(tls_handshakes | jq '[.[]|select(.sni=="expired.kage.test")]|length')"
echo "second fetch: $(echo "$r" | jq -c '{outcome,reason}'); handshakes with expired.kage.test: ${hs}"
[ "$hs" = 1 ]
check "the site is not asked again within the error ttl" $?

echo "== 6. observed: a robots.txt that answers 304 once the kept file expired (F90)"
tls_set '{"host":"www.tlsf4.test","path":"/robots.txt","status":200,"headers":{"content-type":"text/plain","last-modified":"Mon, 02 Nov 2026 10:00:00 GMT"},"body":"User-agent: *\nDisallow: /private\n"}' > /dev/null
tls_set '{"host":"www.tlsf4.test","path":"/a","status":200,"headers":{"content-type":"text/html"},"body":"<html><body>a</body></html>"}' > /dev/null
r="$(fetch "$KEY" '{"url":"https://www.tlsf4.test/a","level":0,"max_retries":0}')"
echo "first: $(echo "$r" | jq -c '{outcome,status}')"
bash "$H" sql "$S" "UPDATE robots_cache SET expires_at = now() - interval '1 second' WHERE origin = 'https://www.tlsf4.test'" > /dev/null
sleep 35
tls_set '{"host":"www.tlsf4.test","path":"/robots.txt","status":304,"headers":{},"body":""}' > /dev/null
r="$(fetch "$KEY" '{"url":"https://www.tlsf4.test/a","level":0,"max_retries":0}')"
echo "after the 304: $(echo "$r" | jq -c '{outcome,reason,status,retry_at}')"
echo "robots_cache: $(bash "$H" sql "$S" "SELECT status, unreachable, body IS NOT NULL, round(extract(epoch FROM expires_at - fetched_at)) FROM robots_cache WHERE origin = 'https://www.tlsf4.test'")"
echo "the request the scout sent: $(tls_log | jq -c '[.[]|select(.sni=="www.tlsf4.test" and .url=="/robots.txt")]|last|{ims:.headers["if-modified-since"],inm:.headers["if-none-match"]}')"

echo "== firewall"
bash "$H" fw | head -n 6
bash "$H" down "$S" > /dev/null
if [ "$failed" = 0 ]; then echo ALL-PASSED; exit 0; fi
echo "FAILED ${failed}"
exit 1
