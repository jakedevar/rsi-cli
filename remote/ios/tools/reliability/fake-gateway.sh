#!/usr/bin/env bash
# Build FakeGateway.swift, make a localhost TLS identity and start the gateway
# in the background (#350). Used by run.sh and simulator.sh.
#
# usage: fake-gateway.sh <work-dir> <port> [ca.pem ca.key]
#   With a CA, the leaf is signed by it (a simulator trusts the CA); without,
#   it is self-signed (the macOS harness accepts it for localhost only).
# Writes <work-dir>/gateway.pid and waits until the gateway listens.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
work=$1
port=$2
swiftc -swift-version 6 -parse-as-library "$here/FakeGateway.swift" -o "$work/fake-gateway"

openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" \
    -keyout "$work/key.pem" -out "$work/leaf.csr" > /dev/null 2>&1
printf 'subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\nbasicConstraints=CA:FALSE\n' > "$work/ext"
if [ $# -ge 4 ]; then
    openssl x509 -req -in "$work/leaf.csr" -CA "$3" -CAkey "$4" -CAcreateserial -days 2 \
        -extfile "$work/ext" -out "$work/cert.pem" > /dev/null 2>&1
else
    openssl x509 -req -in "$work/leaf.csr" -signkey "$work/key.pem" -days 2 \
        -extfile "$work/ext" -out "$work/cert.pem" > /dev/null 2>&1
fi
openssl pkcs12 -export -inkey "$work/key.pem" -in "$work/cert.pem" -out "$work/identity.p12" \
    -passout pass:rsi-reliability -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1 > /dev/null 2>&1

"$work/fake-gateway" "$port" "$work/identity.p12" rsi-reliability > "$work/gateway.log" 2>&1 &
echo $! > "$work/gateway.pid"
for _ in $(seq 1 50); do
    grep -q "fake-gateway: https" "$work/gateway.log" 2> /dev/null && exit 0
    sleep 0.1
done
echo "fake-gateway: did not start" >&2
cat "$work/gateway.log" >&2
exit 1
