#!/usr/bin/env bash
# QA round three, topic level0: after infra/tls/mkcerts.sh, the certificate
# of sixteen more sites (www.l0a.test ... www.l0p.test and their bare names),
# signed by round two's private CA, and the bad CA files the start is asked
# to refuse. Keys stay in /scripts/qa/infra/tls/certs and are never printed.
#
#   bash /scripts/qa/infra/level0/mkcerts0.sh
#
#   l0.pem / l0.key         the sixteen sites (signed by ca, two years)
#   bad-empty.pem           an empty file
#   bad-garbage.pem         text that is no PEM
#   bad-leaf.pem            a leaf (good.pem's certificate), no CA
#   bad-key.pem             a private key only, no certificate (other-ca.key)
#   bad-unreadable.pem      ca.pem with mode 000
#   ca-bundle.pem           other-ca.pem then ca.pem (a bundle of two)
set -euo pipefail
bash /scripts/qa/infra/tls/mkcerts.sh > /dev/null
dir=/scripts/qa/infra/tls/certs
cd "${dir}"
if [ ! -f l0.pem ]; then
    san="$(for c in a b c d e f g h i j k l m n o p; do printf 'DNS:www.l0%s.test,DNS:l0%s.test,' "$c" "$c"; done | sed 's/,$//')"
    openssl req -newkey rsa:2048 -nodes -keyout l0.key -out l0.csr -subj "/CN=www.l0a.test" 2> /dev/null
    printf 'basicConstraints=CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=%s\n' "${san}" > l0.ext
    openssl x509 -req -in l0.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out l0.pem -days 730 -sha256 -extfile l0.ext 2> /dev/null
    rm -f l0.csr l0.ext ./*.srl
fi
: > bad-empty.pem
printf 'this is not a certificate\n' > bad-garbage.pem
cp good.pem bad-leaf.pem
cp other-ca.key bad-key.pem
cp ca.pem bad-unreadable.pem
cat other-ca.pem ca.pem > ca-bundle.pem
chmod 644 ./*.pem ./*.key
chmod 000 bad-unreadable.pem
openssl x509 -in l0.pem -noout -subject -enddate | tr '\n' ' '
echo
