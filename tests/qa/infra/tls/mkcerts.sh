#!/usr/bin/env bash
# QA round two, topic tls: a private certificate authority and the server
# certificates of the world's https names, made at run time with openssl.
#
#   bash /scripts/qa/infra/tls/mkcerts.sh [DIR]     (default /scripts/qa/infra/tls/certs)
#
# Makes what is missing (delete DIR to make everything again):
#   ca.pem / ca.key            "Kage QA TLS Root": the CA a stack is told to trust
#   other-ca.pem / .key        "Kage QA Unknown Root": a CA nobody trusts
#   good.pem / good.key        tls.kage.test, tls2.kage.test, secure.kage.test,
#                              echo.tls.kage.test (signed by ca, valid 2 years)
#   fresh.pem / fresh.key      www.tlsf1.test ... www.tlsf9.test: nine sites of
#                              registrable domains of their own, for level 0
#                              (a site every persona has read has no scout)
#   expired.pem / .key         expired.kage.test (signed by ca, 2020-01 to 2020-02)
#   wrongname.pem / .key       nothere.kage.test, served for wrongname.kage.test (signed by ca)
#   unknownca.pem / .key       unknownca.kage.test (signed by other-ca)
#   selfsigned.pem / .key      selfsigned.kage.test (self-signed, its own root)
# Every leaf is the leaf only (no chain): ca is a root.
# Keys never leave DIR and are never printed.
set -euo pipefail
dir="${1:-/scripts/qa/infra/tls/certs}"
mkdir -p "${dir}"
cd "${dir}"

root() { # name file
    [ -f "$2.pem" ] && return 0
    openssl req -x509 -newkey rsa:2048 -nodes -keyout "$2.key" -out "$2.pem" -days 730 \
        -subj "/CN=$1" -addext "basicConstraints=critical,CA:TRUE" \
        -addext "keyUsage=critical,keyCertSign,cRLSign" 2> /dev/null
}
root "Kage QA TLS Root" ca
root "Kage QA Unknown Root" other-ca

leaf() { # file cn san-list issuer [start end]
    local file="$1" cn="$2" san="$3" issuer="$4"
    [ -f "${file}.pem" ] && return 0
    openssl req -newkey rsa:2048 -nodes -keyout "${file}.key" -out "${file}.csr" -subj "/CN=${cn}" 2> /dev/null
    printf 'basicConstraints=CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=%s\n' "${san}" > "${file}.ext"
    if [ "$#" -ge 6 ]; then
        # openssl 3.0's x509 has no start date: the ca command does.
        rm -rf ca-db && mkdir ca-db && : > ca-db/index.txt && echo 01 > ca-db/serial
        cat > ca-db/ca.cnf <<EOF
[ ca ]
default_ca = qa
[ qa ]
database = ca-db/index.txt
serial = ca-db/serial
new_certs_dir = ca-db
default_md = sha256
policy = any
copy_extensions = none
unique_subject = no
[ any ]
commonName = supplied
EOF
        openssl ca -batch -config ca-db/ca.cnf -cert "${issuer}.pem" -keyfile "${issuer}.key" \
            -in "${file}.csr" -out "${file}.pem" -startdate "$5" -enddate "$6" -extfile "${file}.ext" -notext 2> /dev/null
        rm -rf ca-db
    else
        openssl x509 -req -in "${file}.csr" -CA "${issuer}.pem" -CAkey "${issuer}.key" -CAcreateserial \
            -out "${file}.pem" -days 730 -sha256 -extfile "${file}.ext" 2> /dev/null
    fi
    rm -f "${file}.csr" "${file}.ext"
}
leaf good tls.kage.test "DNS:tls.kage.test,DNS:tls2.kage.test,DNS:secure.kage.test,DNS:echo.tls.kage.test" ca
leaf fresh www.tlsf1.test "$(for i in 1 2 3 4 5 6 7 8 9; do printf 'DNS:www.tlsf%s.test,' "$i"; done | sed 's/,$//')" ca
leaf expired expired.kage.test "DNS:expired.kage.test" ca 20200101000000Z 20200201000000Z
leaf wrongname nothere.kage.test "DNS:nothere.kage.test" ca
leaf unknownca unknownca.kage.test "DNS:unknownca.kage.test" other-ca
if [ ! -f selfsigned.pem ]; then
    openssl req -x509 -newkey rsa:2048 -nodes -keyout selfsigned.key -out selfsigned.pem -days 730 \
        -subj "/CN=selfsigned.kage.test" -addext "subjectAltName=DNS:selfsigned.kage.test" 2> /dev/null
fi
rm -f ./*.srl
chmod 644 ./*.pem ./*.key 2> /dev/null || true
for f in good fresh expired wrongname unknownca selfsigned; do
    printf '%-11s %s\n' "${f}" "$(openssl x509 -in "${f}.pem" -noout -subject -enddate | tr '\n' ' ')"
done
