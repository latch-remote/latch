#!/usr/bin/env bash
# Install a verified release as the user who will own the headless desktop.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ $# -lt 1 || $# -gt 2 || (${2:-} != '' && ${2:-} != --install-only) ]]; then
    echo "usage: $0 /path/to/latch-host [--install-only]" >&2
    exit 2
fi
[[ -x "$1" ]] || { echo "binary must exist and be executable: $1" >&2; exit 1; }
for command in Xvfb xauth mcookie xdpyinfo startxfce4 dbus-run-session pulseaudio; do
    command -v "$command" >/dev/null || { echo "missing runtime command: $command (see README.md)" >&2; exit 1; }
done
if [[ $(id -u) == 0 ]]; then
    echo 'Run as the desktop user, not root.' >&2
    exit 1
fi
echo 'Installing binary at /usr/local/bin/latch-host (sudo)'
sudo install -m 0755 "$1" /usr/local/bin/latch-host
UNITS="$HOME/.config/systemd/user"
echo "Installing headless units in $UNITS (replaces shipped unit names)"
mkdir -p "$UNITS/latch.service.d"
install -m 0644 "$HERE/latch-display.service" "$HERE/latch-desktop.service" "$UNITS/"
install -m 0644 "$HERE/latch.service.d/desktop.conf" "$UNITS/latch.service.d/"
cat > "$UNITS/latch.service" <<'UNIT'
[Unit]
Description=Latch
After=network.target

[Service]
ExecStart="/usr/local/bin/latch-host" --background
Restart=on-failure

[Install]
WantedBy=default.target
UNIT
if [[ ${2:-} == --install-only ]]; then
    echo 'Installed files only; lingering and services were not changed.'
    exit 0
fi
echo 'Enabling lingering (sudo) and starting user services'
sudo loginctl enable-linger "$(id -un)"
systemctl --user daemon-reload
systemctl --user enable latch-display.service latch-desktop.service latch.service
systemctl --user restart latch.service
systemctl --user --no-pager status latch-display.service latch-desktop.service latch.service
