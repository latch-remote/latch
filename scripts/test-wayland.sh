#!/usr/bin/env bash
# Private headless compositor and real ScreenCast portal, with SHM buffers.
set -euo pipefail
if [[ ${1:-} != --session ]]; then
    exec dbus-run-session -- "$0" --session "$@"
fi
shift
binary=$(realpath "${1:-target/debug/examples/wayland_smoke}")
if (( $# > 0 )); then shift; fi
fixture=$(mktemp -d)
export XDG_RUNTIME_DIR="$fixture/runtime"
export XDG_CONFIG_HOME="$fixture/config"
export XDG_STATE_HOME="$fixture/state"
export XDG_DATA_HOME="$fixture/data"
export LATCH_WAYLAND_TOKEN_FILE="$XDG_STATE_HOME/wayland-portal.json"
export XDG_CURRENT_DESKTOP=sway XDG_SESSION_TYPE=wayland
export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_HEADLESS_OUTPUTS=2
export LIBSEAT_BACKEND=noop LATCH_LINUX_BACKEND=wayland LATCH_WAYLAND_DMABUF=0
export RUST_LOG=latch_engine=debug
unset DISPLAY WAYLAND_DISPLAY SWAYSOCK
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/xdg-desktop-portal-wlr" "$XDG_CONFIG_HOME/xdg-desktop-portal"
chmod 700 "$XDG_RUNTIME_DIR"
pids=()
cleanup() {
    result=$?
    trap - EXIT
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
    for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null || true; done
    if (( result != 0 )); then cat "$fixture"/*.log; fi
    if mountpoint -q "$XDG_RUNTIME_DIR/doc"; then fusermount3 -u "$XDG_RUNTIME_DIR/doc"; fi
    rm -rf "$fixture"
    exit "$result"
}
trap cleanup EXIT
wait_for() {
    local pid=$1 i
    shift
    for ((i=0; i<100; i++)); do
        kill -0 "$pid" || return 1
        if "$@" >> "$fixture/readiness.log" 2>&1; then return 0; fi
        sleep 0.1
    done
    echo "timed out waiting for $*" >&2
    return 1
}
owns_bus_name() {
    # Ask the bus daemon itself so the probe cannot activate a system portal.
    dbus-send --session --print-reply --reply-timeout=1000 --dest=org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus.GetConnectionUnixProcessID string:"$2" | grep -q "uint32 $1$"
}
cat > "$fixture/sway.conf" <<'CONFIG'
output HEADLESS-1 mode 640x360
output HEADLESS-1 bg #345678 solid_color
output HEADLESS-1 pos 0 0
output HEADLESS-2 mode 640x360 pos 640 0
CONFIG
cat > "$XDG_CONFIG_HOME/xdg-desktop-portal-wlr/config" <<'CONFIG'
[screencast]
output_name=HEADLESS-1
chooser_type=none
max_fps=30
CONFIG
cat > "$XDG_CONFIG_HOME/xdg-desktop-portal/portals.conf" <<'CONFIG'
[preferred]
default=wlr
CONFIG
sway --unsupported-gpu -c "$fixture/sway.conf" > "$fixture/sway.log" 2>&1 &
pids+=("$!")
for ((i=0; i<100; i++)); do
    for socket in "$XDG_RUNTIME_DIR"/wayland-*; do
        if [[ -S $socket ]]; then export WAYLAND_DISPLAY="${socket##*/}"; break 2; fi
    done
    sleep 0.1
done
: "${WAYLAND_DISPLAY:?headless sway failed to start}"
for socket in "$XDG_RUNTIME_DIR"/sway-ipc*.sock; do export SWAYSOCK="$socket"; done
for ((i=0; i<100; i++)); do
    if grim -o HEADLESS-1 -t ppm "$fixture/ready.ppm" 2> "$fixture/grim.log"; then break; fi
    sleep 0.1
done
test -s "$fixture/ready.ppm"
dbus-update-activation-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_TYPE XDG_RUNTIME_DIR XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME
pipewire > "$fixture/pipewire.log" 2>&1 &
pids+=("$!")
# xdpw exits if PipeWire is still starting. The frontend would then
# D-Bus-activate the system xdpw, losing the SHM fixture patch.
wait_for "$!" timeout 1 pw-cli info 0
wireplumber > "$fixture/wireplumber.log" 2>&1 &
pids+=("$!")
"${LATCH_TEST_PORTAL_WLR:-/usr/libexec/xdg-desktop-portal-wlr}" -l DEBUG > "$fixture/portal-wlr.log" 2>&1 &
pids+=("$!")
export LATCH_TEST_PORTAL_PID="$!"
wait_for "$!" owns_bus_name "$!" org.freedesktop.impl.portal.desktop.wlr
/usr/libexec/xdg-desktop-portal -v > "$fixture/portal.log" 2>&1 &
pids+=("$!")
wait_for "$!" owns_bus_name "$!" org.freedesktop.portal.Desktop
timeout 45 "$binary" "$@"
