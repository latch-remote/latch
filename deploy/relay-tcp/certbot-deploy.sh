#!/usr/bin/env bash
# Install only this relay's renewed identity. Certbot runs deploy hooks as root.
set -euo pipefail
umask 077
lineage=/etc/letsencrypt/live/relay.latch.rune.foo
[[ $(id -u) == 0 ]] || { echo 'Relay certificate deployment requires root.' >&2; exit 1; }
[[ ${RENEWED_LINEAGE:-} == "$lineage" ]] || exit 0
exec 9>/run/latch-relay-cert.lock
flock 9
stage=$(mktemp -d /etc/latch-relay/.renew.XXXXXX)
replacing=0
cleanup() {
    status=$?
    trap - EXIT
    if [[ $status != 0 && $replacing == 1 ]]; then
        cp -p "$stage/old-chain.pem" /etc/latch-relay/fullchain.pem
        cp -p "$stage/old-key.pem" /etc/latch-relay/privkey.pem
        systemctl start latch-relay.service || true
        echo 'Relay certificate deployment failed. Previous identity restored.' >&2
    fi
    rm -rf -- "$stage"
    exit "$status"
}
trap cleanup EXIT
install -o root -g latch-relay -m 0640 "$lineage/fullchain.pem" "$stage/new-chain.pem"
install -o root -g latch-relay -m 0640 "$lineage/privkey.pem" "$stage/new-key.pem"
openssl x509 -in "$stage/new-chain.pem" -noout -checkend 86400 >/dev/null
openssl x509 -in "$stage/new-chain.pem" -noout -checkhost relay.latch.rune.foo >/dev/null
openssl x509 -in "$stage/new-chain.pem" -pubkey -noout > "$stage/cert-public.pem"
openssl pkey -in "$stage/new-key.pem" -passin pass: -pubout > "$stage/key-public.pem" 2>/dev/null
cmp -s "$stage/cert-public.pem" "$stage/key-public.pem"
cp -p /etc/latch-relay/fullchain.pem "$stage/old-chain.pem"
cp -p /etc/latch-relay/privkey.pem "$stage/old-key.pem"
# A planned renewal ends current sessions. Validate everything before stopping,
# then install both files while the service is stopped and retain the ledger.
systemctl stop latch-relay.service
replacing=1
mv -f "$stage/new-chain.pem" /etc/latch-relay/fullchain.pem
mv -f "$stage/new-key.pem" /etc/latch-relay/privkey.pem
systemctl start latch-relay.service
sleep 1
systemctl is-active --quiet latch-relay.service
replacing=0
echo 'Relay TLS identity deployed. Persistent quota retained.'
