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
build_log=$(mktemp)
smoke_dir=$(mktemp -d)
real_unit_started=false
cleanup() {
  local status=$?
  trap - EXIT
  if (( status != 0 )); then
    # Selected scalar properties exclude unit environments. Bound each journal
    # and the number of fixture units, including failures outside Rust waits.
    local unit
    while read -r unit; do
      systemctl --user show --property=LoadState,ActiveState,SubState,Result,MainPID,NRestarts,ExecMainStatus,Job "$unit" >&2 || true
      journalctl --user --unit="$unit" --lines=80 --no-pager --output=short-monotonic 2>&1 | head -c 65536 >&2 || true
    done < <({ echo latch.service; systemctl --user list-units --all --plain --no-legend 'latch-update-*' | awk 'NR <= 8 {print $1}'; } | sort -u)
  fi
  if [[ $real_unit_started == true ]]; then
    systemctl --user stop latch.service >/dev/null 2>&1 || true
  fi
  rm -f "$log" "$build_log"
  rm -rf "$smoke_dir"
  exit "$status"
}
trap cleanup EXIT
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
assert "PASS definitive systemd failure rolled back within 15 seconds" in output, output
assert len(re.findall(r"^test result: ok\. 1 passed; 0 failed; 0 ignored;", output, re.MULTILINE)) == 1, output
assert not re.search(r"^test result:.*\b0 passed;", output, re.MULTILINE), output
assert "SKIP" not in output and "REFUSE" not in output, output
PY

# Also start the production executable under the same user manager. This is a
# startup/status smoke check with isolated settings and no desktop session.
cargo build --locked -p latch-host --bin latch-host -j 4 --message-format=json > "$build_log"
real_exe=$(python3 - "$build_log" <<'PYBUILD'
import json
import sys
from pathlib import Path
artifacts = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines()]
print(next(item["executable"] for item in artifacts if item.get("reason") == "compiler-artifact" and item.get("target", {}).get("name") == "latch-host" and item.get("executable")))
PYBUILD
)
# The fixture must have removed its unit and released the fixed control port.
[[ $(systemctl --user show --property=LoadState --value latch.service) == not-found ]]
mkdir -p "$smoke_dir/data"
printf 'start_with_windows = false\nstay_awake = false\n' > "$smoke_dir/data/host.toml"
real_unit_started=true
systemd-run --user --collect --quiet --service-type=exec --unit=latch.service \
  --property=RuntimeMaxSec=30 --property=TimeoutStopSec=5 \
  --setenv="LATCH_DATA_DIR=$smoke_dir/data" --setenv="HOME=$smoke_dir" \
  --setenv="XDG_CONFIG_HOME=$smoke_dir/config" "$real_exe" --background
python3 - "$real_exe" <<'PYSMOKE'
import json
import os
import subprocess
import sys
import time
import urllib.request
exe = sys.argv[1]
expected = subprocess.check_output([exe, "--version"], text=True).strip().split()[-1]
deadline = time.monotonic() + 20
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
while True:
    try:
        with opener.open("http://127.0.0.1:47850/v1/status", timeout=0.5) as response:
            status = json.load(response)
        assert status["version"] == expected
        pid = int(subprocess.check_output(["systemctl", "--user", "show", "--property=MainPID", "--value", "latch.service"], text=True).strip())
        installed = os.stat(exe)
        running = os.stat(f"/proc/{pid}/exe")
        assert (running.st_dev, running.st_ino) == (installed.st_dev, installed.st_ino)
        break
    except (OSError, ValueError, AssertionError):
        if time.monotonic() >= deadline:
            raise
        time.sleep(0.1)
print("PASS real latch-host systemd startup and matching status version")
PYSMOKE
systemctl --user stop latch.service
real_unit_started=false
