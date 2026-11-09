#!/bin/sh
set -eu
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH" TMPDIR=/tmp
run() {
  if [ -n "${PB_TEST_BIN:-}" ]; then "$PB_TEST_BIN" paced_socket_measurement --ignored --nocapture;
  else cargo test --release -p latch-engine --lib paced_socket_measurement -- --ignored --nocapture; fi
}
for cap in 5000 20000 50000; do
  for rtt in 20 150; do
    PB_CAP=$cap PB_RTT=$rtt run
  done
done
for request in 100000 1000000; do
  PB_CAP=0 PB_RTT=0 PB_REQUEST=$request run
done
