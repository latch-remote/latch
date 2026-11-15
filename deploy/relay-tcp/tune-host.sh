#!/usr/bin/env bash
# Dedicated Ubuntu relay host profile. Keeps OS/security/management services.
set -euo pipefail
kit=$(cd -- "$(dirname -- "$0")" && pwd)
bash "$kit/network.sh"

# This AWS VM has one NVMe disk, no modem, removable media or host firmware.
for unit in ModemManager.service multipathd.service multipathd.socket \
    udisks2.service fwupd.service fwupd-refresh.service fwupd-refresh.timer; do
    if systemctl cat "$unit" >/dev/null 2>&1; then
        systemctl disable --now "$unit"
        systemctl mask "$unit"
    fi
done
# Ubuntu images use either service- or socket-activated SSH and different clocks.
if ! systemctl is-active --quiet ssh.service ssh.socket; then
    echo 'Warning: SSH service/socket is inactive.' >&2
fi
if ! systemctl is-active --quiet chrony.service systemd-timesyncd.service; then
    echo 'Warning: no supported time-sync service is active.' >&2
fi
systemctl is-active unattended-upgrades.service
systemctl is-enabled certbot.timer apt-daily.timer apt-daily-upgrade.timer
