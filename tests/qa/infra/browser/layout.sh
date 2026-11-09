#!/usr/bin/env bash
# QA round four: the keyboard a persona of a declared country and locale types
# on. Brings up a stack of its own under `egress.mode: direct` with
# `egress.direct.country` and `egress.direct.locale` (three host personas),
# types TEXT (default: a sample of letters, digits and symbols) with each
# engine into keys4.html, and prints keys4-analyze.mjs's report per engine:
# the layout map Chrome reports, the code each character came on, typos.
#   bash layout.sh COUNTRY LOCALE [TEXT]
# Environment: TYPOS (false), KEEP=1 leaves the stack up, ENGINES ("chromium firefox").
set -uo pipefail
C="${1:?country}"
LOC="${2:?locale}"
DEFAULT_TEXT="azertyuiop qwertz yxcvbnm; 1234567890 @#&é(-è_çà)= ?./§ !:,;"
TEXT="${3:-$DEFAULT_TEXT}"
H=/scripts/qa/stack.sh
stack="bl-$(echo "$C-$LOC" | tr 'A-Z' 'a-z')-$(date +%s | tail -c 5)"
bash $H up "$stack" QA_PERSONAS=3 \
    KAGE_E2E_EGRESS_MODE=direct \
    KAGE_E2E_PRIVATE_NAMES='["fixture.kage.test","world.kage.test","qa.kage.test","other.qa.test"]' \
    KAGE__EGRESS__DIRECT__COUNTRY="$C" KAGE__EGRESS__DIRECT__LOCALE="$LOC" \
    KAGE__EGRESS__DIRECT__PERSONAS=3 > "/tmp/$stack-up.log" 2>&1 || {
    echo "stack $stack did not come up"
    tail -n 20 "/tmp/$stack-up.log"
    bash $H down "$stack" > /dev/null 2>&1
    exit 2
}
bash $H fast "$stack" > /dev/null 2>&1
echo "personas: $(bash $H cli "$stack" persona list --json | jq -c '[.personas[] | {engine, country, locale, ready}]')"
for e in ${ENGINES:-chromium firefox}; do
    OUT="/tmp/$stack-$e" TYPOS="${TYPOS:-false}" bash /scripts/qa/infra/browser/type4.sh "$stack" "$e" "$TEXT" |
        jq -c '{engine, job, env, same: .input.same, got: .input.got, lost: .input.diff.lost, codeOf, typos: {n: .typos.n, notNeighbours: .typos.notNeighbours, list: .typos.list}, capitalsWithoutShift, pressesWithNoCode: (.pressesWithNoCode | length), deadOrProcess}'
done
[ "${KEEP:-0}" = 1 ] && { echo "stack $stack kept"; exit 0; }
bash $H down "$stack" > /dev/null 2>&1
echo "stack $stack down"
