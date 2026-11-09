#!/bin/sh
set -eu
# Build: cargo test --release -p latch-engine --test bottleneck --no-run
# Pass the executable Cargo prints as argument 1.
for cap in 5000 20000 50000; do
  for rtt in 20 150; do
    PB_CAP=$cap PB_RTT=$rtt "$1" noise_socket_measurement --ignored --nocapture
done
done
for request in 100000 1000000; do
  PB_CAP=0 PB_RTT=0 PB_REQUEST=$request "$1" noise_socket_measurement --ignored --nocapture
done
