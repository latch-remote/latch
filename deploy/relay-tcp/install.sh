#!/usr/bin/env bash
set -euo pipefail
umask 077
kit=$(cd -- "$(dirname -- "$0")" && pwd)
die() { echo "install relay: $*" >&2; exit 1; }
[[ $(uname -s) == Linux && $(id -u) == 0 ]] || die 'run as root on Linux'
[[ $# == 4 || ( $# == 5 && $5 == --initialize-quota ) ]] ||
    die 'usage: install.sh BINARY FULLCHAIN_PEM PRIVATE_KEY_PEM MONTHLY_BYTES [--initialize-quota]'
binary=$1 cert=$2 key=$3 budget=$4
[[ $budget =~ ^[1-9][0-9]{0,17}$ && $budget -ge 65536 ]] || die 'budget must be 65536..999999999999999999 bytes'
[[ -f $binary && -x $binary && -s $cert && -s $key ]] || die 'binary, certificate or key missing'
for command in systemctl setpriv timeout openssl getent useradd groupadd install cmp; do
    command -v "$command" >/dev/null || die "missing prerequisite $command"
done
systemctl is-active --quiet latch-relay.service && die 'stop the relay during an arranged maintenance window before installing'
# Validate TLS identity before replacing installed files. Never print key data.
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT
openssl x509 -in "$cert" -noout -checkend 86400 >/dev/null || die 'certificate expires within 24 hours or is invalid'
openssl x509 -in "$cert" -pubkey -noout > "$tmp/cert-public.pem"
openssl pkey -in "$key" -passin pass: -pubout > "$tmp/key-public.pem" 2>/dev/null || die 'key must be a valid unencrypted PEM'
cmp -s "$tmp/cert-public.pem" "$tmp/key-public.pem" || die 'certificate and key do not match'
"$binary" --help > "$tmp/help"
grep -q -- '--http-listen' "$tmp/help" || die 'binary lacks the relay CLI contract'
if getent passwd latch-relay >/dev/null; then
    [[ $(id -u latch-relay) != 0 && $(id -gn latch-relay) == latch-relay ]] || die 'existing service account has unexpected identity'
    shell=$(getent passwd latch-relay | cut -d: -f7)
    [[ $shell == */nologin || $shell == */false ]] || die 'existing service account permits login'
else
    getent group latch-relay >/dev/null || groupadd --system latch-relay
    useradd --system --gid latch-relay --home-dir /var/lib/latch-relay --no-create-home --shell /usr/sbin/nologin latch-relay
fi
[[ ! -L /var/lib/latch-relay && ! -L /etc/latch-relay ]] || die 'service directories must not be symlinks'
install -d -o root -g latch-relay -m 0750 /etc/latch-relay
install -d -o latch-relay -g latch-relay -m 0700 /var/lib/latch-relay
[[ ! -L /var/www/latch-relay ]] || die 'ACME webroot must not be a symlink'
install -d -o root -g latch-relay -m 0750 /var/www/latch-relay /var/www/latch-relay/.well-known /var/www/latch-relay/.well-known/acme-challenge
if [[ $# == 5 ]]; then
    [[ ! -e /var/lib/latch-relay/quota && ! -L /var/lib/latch-relay/quota ]] || die 'quota already exists. Retain the ledger on upgrades'
fi
# Rename the executable to avoid an interrupted copy replacing a working binary.
install -o root -g root -m 0755 "$binary" /usr/local/bin/latch-relay.new
mv -f /usr/local/bin/latch-relay.new /usr/local/bin/latch-relay
install -o root -g latch-relay -m 0640 "$cert" /etc/latch-relay/fullchain.pem
install -o root -g latch-relay -m 0640 "$key" /etc/latch-relay/privkey.pem
# Local operator limits survive a reinstall.
if [[ ! -e /etc/latch-relay/limits.json ]]; then
    install -o root -g latch-relay -m 0640 "$kit/limits.json" /etc/latch-relay/limits.json
fi
printf 'MONTHLY_BYTES=%s\n' "$budget" > /etc/latch-relay/budget.conf
chown root:latch-relay /etc/latch-relay/budget.conf
chmod 0640 /etc/latch-relay/budget.conf
install -o root -g root -m 0644 "$kit/latch-relay.service" /etc/systemd/system/latch-relay.service
if [[ $# == 5 ]]; then
    # The current CLI initializes then serves. Bind a disposable loopback port
    # as the unprivileged service account, and terminate after initialization.
    status=0
    timeout --signal=TERM --kill-after=2s 2s setpriv \
        --reuid=latch-relay --regid=latch-relay --init-groups --no-new-privs -- \
        /usr/local/bin/latch-relay --listen 127.0.0.1:0 \
        --cert /etc/latch-relay/fullchain.pem --key /etc/latch-relay/privkey.pem \
        --quota-file /var/lib/latch-relay/quota --monthly-bytes "$budget" \
        --limits-json /etc/latch-relay/limits.json --initialize-quota || status=$?
    [[ $status == 124 && -f /var/lib/latch-relay/quota && $(stat -c %s /var/lib/latch-relay/quota) == 32 ]] ||
        die 'quota initialization failed. Inspect the error and retain any created ledger'
fi
systemctl daemon-reload
systemctl enable latch-relay.service
echo 'Installed and enabled. Start explicitly with systemctl start latch-relay.service after checking IPv4/IPv6 firewall, DNS, TLS and quota.'
