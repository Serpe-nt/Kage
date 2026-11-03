#!/usr/bin/env bash
# Writes an extra.yaml for a running stack: the responder also answers s1..s8.qa.test
# (and the proxies relay them, with one trailing dot too), then recreates those services.
# usage: mk-extra.sh STACK
set -uo pipefail
stack="${1:?stack}"
names="qa.kage.test other.qa.test"
for i in 1 2 3 4 5 6 7 8; do names="$names s$i.qa.test"; done
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
} > "/target/qa/runs/$stack/extra.yaml"
bash /scripts/qa/stack.sh dc "$stack" up --detach --wait responder proxy-a proxy-b proxy-c 2>&1 | tail -3
