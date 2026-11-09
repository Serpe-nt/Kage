#!/usr/bin/env bash
# QA round four sched: writes names4.yaml beside itself: names3 plus sc1..sc8.test, each a registrable domain of its own (fresh sites for robots scouts), a compose overlay given at `stack.sh up`
# (--overlay=/scripts/qa/infra/sched/names4.yaml) so that the proxies are never
# recreated: the responder answers s1..s8.qa.test and n1..n6.qa.test too, and
# the proxies relay them (with one trailing dot).
set -uo pipefail
names="qa.kage.test other.qa.test"
for i in 1 2 3 4 5 6 7 8; do names="$names s$i.qa.test"; done
for i in 1 2 3 4 5 6; do names="$names n$i.qa.test"; done
for i in 1 2 3 4 5 6 7 8; do names="$names sc$i.test"; done
A='fixture.kage.test:8088,lenient.kage.test:8088,rules.kage.test:8088,wall.field.test:8088,world.kage.test:8089,fixture.kage.test.:8088'
for n in $names; do A="$A,$n:8091,$n.:8091"; done
{
  echo 'services:'
  echo '  responder:'
  echo '    networks:'
  echo '      default:'
  printf '        aliases: [%s]\n' "$(echo $names | sed 's/ /, /g')"
  for pair in a:provider-a:e2e-only-a b:provider-b:e2e-only-b c:provider-c:e2e-only-c; do
    IFS=: read -r n u p <<< "$pair"
    echo "  proxy-$n:"
    echo "    command: [/e2e/world.mjs, proxy, --port, \"3128\", --user, $u, --pass, $p, --allow, \"$A\"]"
  done
} > "$(dirname "$0")/names4.yaml"
