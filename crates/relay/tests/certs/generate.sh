#!/bin/sh
# Test-only localhost identity. Never deploy this published private key.
set -eu
cd "$(dirname "$0")"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 -subj '/CN=Latch Relay Test CA' -addext 'basicConstraints=critical,CA:TRUE' -keyout ca-key.tmp -out ca.pem
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj '/CN=localhost' -keyout server-key.pem -out server.tmp
cat > extensions.tmp <<'EXT'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost
EXT
openssl x509 -req -in server.tmp -CA ca.pem -CAkey ca-key.tmp -CAcreateserial -days 3650 -extfile extensions.tmp -out server.pem
rm -f ca-key.tmp ca.srl server.tmp extensions.tmp
