#!/usr/bin/env bash
set -euo pipefail
# GitHub's Ubuntu runners have systemd as PID 1. The runner user's manager
# may need lingering and an explicit runtime directory in non-login jobs.
export PATH="$HOME/.cargo/bin:$PATH"
export XDG_RUNTIME_DIR="/run/user/$(id -u)"
export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
if ! systemctl --user show-environment >/dev/null 2>&1; then
  sudo -n loginctl enable-linger "$(id -un)"
  sudo -n systemctl start "user@$(id -u).service"
fi
if ! systemctl --user show-environment >/dev/null 2>&1; then
  echo 'SKIP real systemd Linux updater integration: no reachable user manager after enabling lingering' >&2
  if [[ ${CI:-} == true ]]; then
    echo 'CI must execute the integration test on its Ubuntu systemd runner' >&2
    exit 1
  fi
  exit 0
fi
export LATCH_REQUIRE_SYSTEMD_TEST=1
cargo test --locked -p latch-host --lib -j 4 linux_systemd_update_survives_restart_rolls_back_and_recovers -- --nocapture
