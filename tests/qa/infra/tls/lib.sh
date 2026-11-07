# QA round two, topic tls: helpers for scripts against a stack with
# compose.tls.yaml. Source it: `source /scripts/qa/infra/tls/lib.sh STACK`.
#   tls_set JSON        program the tls responder (both schemes, one state)
#   tls_log             its request log (JSON)
#   tls_handshakes      the handshakes clients broke off (JSON)
#   tls_reset           forget routes and logs
#   tls_check URL       from the world container, GET URL trusting only the
#                       private CA: prints status or the TLS error
#   fetch TOKEN JSON    POST /v1/fetch; prints the body only
#   proxy_stats         each proxy's /__stats targets (JSON lines)
STACK="${1:?stack name}"
H=/scripts/qa/stack.sh
tls_world() { bash "$H" dc "$STACK" exec -T world node -e "$1"; }
tls_set() {
    printf '%s' "$1" | bash "$H" dc "$STACK" exec -T world node -e "let t='';process.stdin.on('data',(c)=>t+=c).on('end',()=>fetch('http://tls.kage.test:80/__set',{method:'POST',body:t}).then((r)=>r.text()).then(console.log,(e)=>{console.error(String(e));process.exit(1)}))"
}
tls_log() { tls_world "fetch('http://tls.kage.test:80/__log').then((r)=>r.text()).then(console.log)"; }
tls_handshakes() { tls_world "fetch('http://tls.kage.test:80/__handshakes').then((r)=>r.text()).then(console.log)"; }
tls_reset() { tls_world "fetch('http://tls.kage.test:80/__reset',{method:'POST'}).then((r)=>r.text()).then(console.log)"; }
tls_check() {
    bash "$H" dc "$STACK" exec -T tlsworld node -e "
const https=require('https');const fs=require('fs');const u=new URL(process.argv[1]);
const req=https.request({host:'tls.kage.test',servername:u.hostname,port:u.port||443,path:u.pathname,headers:{host:u.host},ca:fs.readFileSync('/qa/infra/tls/certs/ca.pem')},(r)=>{console.log(u.hostname,'status',r.statusCode);r.resume();});
req.on('error',(e)=>console.log(u.hostname,'error',e.code||e.message));req.end();" "$1"
}
fetch() { bash "$H" call "$STACK" POST /v1/fetch "$1" "$2" | tail -n 1; }
proxy_stats() {
    for p in proxy-a proxy-b proxy-c; do
        printf '%s ' "$p"
        bash "$H" dc "$STACK" exec -T world node -e "fetch('http://$p:3128/__stats').then((r)=>r.text()).then(console.log)"
    done
}
