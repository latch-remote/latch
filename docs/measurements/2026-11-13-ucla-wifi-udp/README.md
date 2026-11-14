# UCLA campus Wi-Fi: UDP paths lose 15–18%, TCP doesn't

Measured 2026-11-13 by copying a Windows PC's backup to a Mac. This was
not a Latch run, but it is likely the root cause of poor Latch sessions
from campus: Latch rides Tailscale, and every fast Tailscale path is UDP.

## Setup

- **Viewer side:** MacBook on UCLA campus Wi-Fi, Los Angeles.
- **Host side:** the RX 6600 Windows PC on a home connection in Korea.
  Its upload is at least 37.7 MB/s (measured with parallel uploads).
- Both on the same tailnet. No direct path forms: hole punching fails from
  the campus network, so Tailscale falls back to a relay.

## Results

| Path, Mac ⇄ PC | Transport on the campus leg | Throughput | Notes |
| --- | --- | --- | --- |
| Tailscale DERP (Tokyo) | TCP (DERP over HTTPS) | ~0.4 MB/s (~3 Mbps) | Tailscale's default fallback |
| Tailscale peer relay on AWS Seoul, UDP 41641 | UDP (WireGuard) | 1–1.5 MB/s | 15% and 18% ping loss (2 × 100 pings over the tailnet, with the backup running) |
| same relay moved to UDP 443 | UDP | no better | so it isn't one blocked port |
| SSH jump host on the same Seoul instance, then the tailnet to the PC | TCP | 7.5–9.3 MiB/s with 10 parallel streams, ~1 MB/s per stream | ~157–175 ms round trip |

Other figures from the same session:

- **The Mac's total downlink on campus Wi-Fi:** about 14 MB/s. The jump host
  path reached about 10–14 MB/s once more streams were added, so that path
  was limited by the Wi-Fi, not by the PC or AWS.
- **The PC's TCP upload:** 9.2 MB/s on one stream and 17 MB/s over SSH from
  a San Jose host, both fine.
- **The PC's side of the relay:** the PC used the peer relay without trouble,
  while the Mac kept falling back to DERP. Loss on the PC's leg alone wasn't
  measured separately.

## Interpretation

- **What was measured:** loss on the UDP path from the Mac, while the same
  instance carried TCP from the Mac at many times the throughput.
- **What wasn't:** which hop drops the packets (the campus Wi-Fi, the campus
  edge, or rate limiting of UDP), and whether loss is lower without competing
  load. The loss was measured with the backup saturating the link.

## What this means for Latch

- **On campus, Latch never gets a direct path.** It gets one of two paths, and
  both are bad:
  - DERP, at about 3 Mbps in this test;
  - a UDP peer relay, losing 15–18%. That's above the 12% random loss in the
    [recovery measurement](../2026-11-03/rfi/review-fixes/README.md), and
    bursty loss on a ~170 ms round trip is the case `PERFORMANCE.md` already
    calls a weakness.
- **A peer relay doesn't fix it.** It moves traffic off DERP's throughput cap,
  but onto lossy UDP.
- **Quality tiers should account for this.** Auto quality chooses its tier
  by path type (peer relay vs DERP). A peer relay on a network like this one
  reports "peer relay" while behaving worse than the round trip suggests, so
  the tier should come from measured loss as well as path type.
- **A TCP fallback or a TCP-friendly relay would help,** since TCP on the same
  network was fine. Untested with Latch.
- **Testing from campus doesn't reflect normal networks.** Treat campus results
  as a hostile-network case, not a representative one.
