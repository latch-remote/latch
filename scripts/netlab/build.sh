#!/usr/bin/env bash
set -euo pipefail
cargo build --locked -j 4 -p latch-engine --example serve -p latch-stream --example view
cargo build --locked -j 4 -p latch-relay --bin latch-relay
bash scripts/audit-native.sh
