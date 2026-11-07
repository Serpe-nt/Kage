#!/usr/bin/env bash
# Program and read the bgcalls world (overlay-sink.yaml) of a stack.
#   sink.sh NAME program [NET]   Google's and Mozilla's zones answer NET.30 (default 11.66.77)
#   sink.sh NAME log             the sink's connections (JSON lines)
#   sink.sh NAME dnslog          the DNS server's questions (JSON lines)
#   sink.sh NAME reset           forget both logs (the zones stay)
set -uo pipefail
name="$1"; what="$2"; net="${3:-11.66.77}"
in_dns() { docker exec -i "qa-${name}-dns-1" node --input-type=module -e "$1"; }
in_sink() { docker exec -i "qa-${name}-sink-1" node --input-type=module -e "$1"; }
case "${what}" in
program)
    for zone in google.com googleapis.com gstatic.com gvt1.com gvt2.com googleusercontent.com doubleclick.net youtube.com mozilla.org mozilla.net mozilla.com openh264.org mozaws.net firefox.com; do
        in_dns "await fetch('http://127.0.0.1:8053/__set',{method:'POST',body:JSON.stringify({name:'*.${zone}',a:['${net}.30'],aaaa:[],ttl:0})}).then(r=>r.text())" > /dev/null
        in_dns "await fetch('http://127.0.0.1:8053/__set',{method:'POST',body:JSON.stringify({name:'${zone}',a:['${net}.30'],aaaa:[],ttl:0})}).then(r=>r.text())" > /dev/null
    done
    echo programmed
    ;;
log) in_sink "const l=await (await fetch('http://127.0.0.1:8055/__log')).json(); for (const e of l) console.log(JSON.stringify(e))" ;;
dnslog) in_dns "const l=await (await fetch('http://127.0.0.1:8053/__log')).json(); for (const e of l) console.log(JSON.stringify(e))" ;;
reset)
    in_sink "await fetch('http://127.0.0.1:8055/__reset',{method:'POST'})"
    in_dns "await fetch('http://127.0.0.1:8053/__log').then(r=>r.text())" > /dev/null
    ;;
esac
