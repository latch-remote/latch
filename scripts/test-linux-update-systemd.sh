#!/usr/bin/env bash
set -euo pipefail
# Only this dedicated job opts into the fixture's user-unit and fixed port.
export PATH="$HOME/.cargo/bin:$PATH"
export XDG_RUNTIME_DIR="/run/user/$(id -u)"
export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
if ! systemctl --user show-environment >/dev/null 2>&1; then
  sudo -n loginctl enable-linger "$(id -un)"
  sudo -n systemctl start "user@$(id -u).service"
fi
if ! systemctl --user show-environment >/dev/null 2>&1; then
  echo 'SKIP real systemd Linux updater integration because no user manager is reachable' >&2
  if [[ ${CI:-} == true ]]; then exit 1; fi
  exit 0
fi
unit_state=$(systemctl --user show --property=LoadState --value latch.service)
if [[ $unit_state != not-found ]]; then
  echo 'REFUSE real systemd fixture because a latch.service user unit already exists' >&2
  exit 1
fi
export LATCH_REQUIRE_SYSTEMD_TEST=1
log=$(mktemp)
trap 'python3 -c "import os,sys; os.unlink(sys.argv[1])" "$log"' EXIT
cargo test --locked -p latch-host --lib -j 4 update::linux::tests::linux_systemd_update_survives_restart_rolls_back_and_recovers -- --ignored --exact --nocapture 2>&1 | tee "$log"
python3 - "$log" <<'PY'
from pathlib import Path
import re
import sys
output = re.sub(r"\x1b\[[0-9;]*m", "", Path(sys.argv[1]).read_text())
for version, scenario in [("999.0.0", "healthy"), ("999.0.1", "wrong-version"), ("999.0.2", "killed-supervisor")]:
    assert len(re.findall(rf"^PASS real systemd update {re.escape(version)} \({scenario}\),", output, re.MULTILINE)) == 1, output
assert len(re.findall(r"^PASS real systemd update ", output, re.MULTILINE)) == 3, output
assert "PASS crash-on-start replacement, start limit reached, previous service restored" in output, output
assert "PASS running-service monitor recovery, previous service restored" in output, output
assert len(re.findall(r"^test result: ok\. 1 passed; 0 failed; 0 ignored;", output, re.MULTILINE)) == 1, output
assert not re.search(r"^test result:.*\b0 passed;", output, re.MULTILINE), output
assert "SKIP" not in output and "REFUSE" not in output, output
PY
