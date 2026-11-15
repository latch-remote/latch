#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
repo=$(pwd)
image="latch-netlab:local-$$"
container="latch-netlab-$$"
revision=$(git rev-parse HEAD)
results_run="run-$(date -u +%Y%m%dT%H%M%SZ)-$$"
mkdir -p target/netlab
cleanup() {
  docker rm -f "$container" >/dev/null 2>&1 || true
  docker image rm "$image" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM
# Named caches belong to netlab. Other agents' images and build trees are untouched.
docker build -t "$image" scripts/netlab
docker run --name "$container" --privileged --cpus 4 --memory 6g \
  -e NETLAB_VALIDATE="${NETLAB_VALIDATE:-0}" \
  -e NETLAB_REVISION="$revision" -e CARGO_BUILD_JOBS=4 -e NETLAB_RESULTS_RUN="$results_run" \
  -v "$repo:/work" \
  -v latch-netlab-cargo:/root/.cargo/registry \
  -v latch-netlab-target:/work/target \
  -v "$repo/target/netlab:/results" \
  "$image" bash -c '
    set -euo pipefail
    if [[ "$NETLAB_VALIDATE" == 1 ]]; then
      cargo fmt --all -- --check
      cargo clippy --locked --workspace --all-targets -j 4 -- -D warnings
      shellcheck scripts/netlab/*.sh
      python3 -m unittest discover -s scripts/netlab -p "test_*.py" -v
    fi
    if [[ "${1:-}" == --carrier-comparison ]]; then
      shift
      cargo build --locked --release -j 4 -p latch-relay --bin latch-relay --example paired_probe
      python3 scripts/netlab/carrier_comparison.py --output "/results/$NETLAB_RESULTS_RUN" "$@"
    else
      bash scripts/netlab/build.sh
      python3 scripts/netlab/netlab.py --output "/results/$NETLAB_RESULTS_RUN" "$@"
    fi
  ' bash "$@"
