#!/usr/bin/env bash
# QA http topic: writes /target/qa/runs/$1/extra.yaml giving the responder 40
# more names (r1..r40.rb.qa.test, on 8091 and 8092) that the proxies relay,
# for robots.txt work that needs a fresh origin per case; then recreates the
# responder and the proxies. Usage: bash extra.sh STACK
set -euo pipefail
s="$1"
names=""; allow="fixture.kage.test:8088,lenient.kage.test:8088,rules.kage.test:8088,wall.field.test:8088,world.kage.test:8089,qa.kage.test:8091,other.qa.test:8091,qa.kage.test:8092,other.qa.test:8092,third.qa.test:8091"
for i in $(seq 1 40); do names="${names}, r${i}.rb.qa.test"; allow="${allow},r${i}.rb.qa.test:8091,r${i}.rb.qa.test:8092"; done
{
  echo "services:"
  echo "  responder:"
  echo "    command: [/qa/infra/http/multi.mjs]"
  echo "    networks:"
  echo "      default:"
  echo "        aliases: [qa.kage.test, other.qa.test, third.qa.test${names}]"
  for p in a b c; do
    echo "  proxy-${p}:"
    echo "    command: [/e2e/world.mjs, proxy, --port, \"3128\", --user, provider-${p}, --pass, e2e-only-${p}, --allow, \"${allow}\"]"
  done
} > "/target/qa/runs/${s}/extra.yaml"
bash /scripts/qa/stack.sh dc "${s}" up --detach --wait responder proxy-a proxy-b proxy-c
