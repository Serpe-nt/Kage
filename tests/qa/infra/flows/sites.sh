#!/usr/bin/env bash
# The flows agent's sites s1..s8.qa.test on a stack that ran mk-extra.sh:
# domain rules (appended once) and the responder's robots.txt and pages.
# usage: sites.sh STACK [--no-rules]
set -uo pipefail
stack="${1:?stack}"
H=/scripts/qa/stack.sh
if [ "${2:-}" != --no-rules ]; then
bash $H add-domains "$stack" << 'EOF'
  # Added by the flows agent.
  s1.qa.test: { robots: enforce, pacing: fast, min_dwell: 500ms, active_hours: ignore }
  s2.qa.test: { robots: enforce, pacing: fast, min_dwell: 500ms, active_hours: ignore }
  s3.qa.test: { robots: enforce, pacing: fast, min_dwell: 500ms, active_hours: ignore }
  s4.qa.test: { robots: enforce, pacing: fast, min_dwell: 500ms, active_hours: ignore }
  s5.qa.test: { robots: enforce, pacing: fast, min_dwell: 500ms, active_hours: ignore }
  s6.qa.test: { pacing: fast, min_dwell: 500ms, active_hours: ignore, breaker_threshold: 1, breaker_pause: 20m, retries: 0 }
  s7.qa.test: { pacing: fast, min_dwell: 500ms, active_hours: ignore, engines: [firefox] }
  s8.qa.test: { pacing: fast, min_dwell: 500ms, active_hours: ignore, countries: [US] }
EOF
fi
r() { bash $H responder "$stack" "$1" > /dev/null; }
txt() { jq -cn --arg h "$1" --arg p "$2" --arg b "$3" --argjson s "${4:-200}" '{host:$h,path:$p,status:$s,headers:{"content-type":"text/plain"},body:$b}'; }
page='<html><head><title>A page</title></head><body><main><h1>A page of the site</h1><p>Words a reader reads on this page of the site, a paragraph long enough to be content and not a blank page at all, with more words.</p><a id=next href="/next">next</a></main></body></html>'
for i in 1 2 3 4 5 6 7 8; do
    r "$(jq -cn --arg h "s$i.qa.test" --arg b "$page" '{host:$h,path:"*",status:200,headers:{"content-type":"text/html; charset=utf-8"},body:$b}')"
done
r "$(txt s1.qa.test /robots.txt $'User-agent: *\nDisallow: /private\n')"
r "$(txt s2.qa.test /robots.txt 'server error' 500)"
r "$(txt s3.qa.test /robots.txt 'not here' 404)"
r '{"host":"s4.qa.test","path":"/robots.txt","status":301,"headers":{"location":"http://s4.qa.test:8091/robots-moved.txt"},"body":""}'
r "$(txt s4.qa.test /robots-moved.txt $'User-agent: *\nDisallow: /private\n')"
r "$(txt s5.qa.test /robots.txt $'User-agent: *\nCrawl-delay: 90\nDisallow: /private\n')"
r "$(txt s6.qa.test /robots.txt 'none' 404)"
r "$(jq -cn --arg b '<html><head><title>Forbidden</title></head><body>Access denied</body></html>' '{host:"s6.qa.test",path:"/wall",status:403,headers:{"content-type":"text/html"},body:$b}')"
echo "sites ready on $stack"
