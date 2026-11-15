#!/usr/bin/env bash
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y --no-install-recommends \
  build-essential git ca-certificates curl pkg-config libasound2-dev \
  libxkbcommon-dev libwayland-dev libx11-dev libxcursor-dev libxrandr-dev \
  libxi-dev libx11-xcb-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  python3 iputils-ping iproute2 nftables ethtool openssl tinyproxy tayga procps shellcheck \
  pulseaudio pulseaudio-utils libasound2-plugins
rm -rf /var/lib/apt/lists/*
