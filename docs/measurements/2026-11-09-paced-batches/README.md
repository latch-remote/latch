# Paced batches, 2026-11-09

What changed in the sender, and what it measured.

- **Paced batches.** A paced session used to send one datagram per system
  call and sleep between them, which bypassed the batched and offloaded send
  paths (`sendmmsg`/GSO on Linux, USO on Windows). It now sends a few
  milliseconds' worth of datagrams per batch through those paths.
- **Probes on the video sender.** Probe trains go out through the same
  pacer and are counted against the same budget.
- **Backpressure before capture.** A paced session does not take a new
  picture while an encoded frame is already waiting behind the one being
  sent.
- **A bound on frame size**, with an explicit error instead of a frame the
  wire format cannot describe.
- **The pace on an unmeasured path** (found while measuring the above, and
  older than it): the controller no longer paces a stream below twice what
  the viewer is receiving. [Its own section](#the-pace-on-an-unmeasured-path)
  has the cause and the numbers.

No wire format changed. Sessions between this build and 5.1.0 and 5.2.0
were run in both directions (`compat.py`) and all connected and streamed.

## Machine and method

An Apple M3 Pro laptop (12 cores, macOS), in use for other work during the
runs: load average between 3 and 6. Everything is over loopback. Nothing
here is a measurement of a real network, a network card, or of Linux or
Windows.

Two harnesses, both ignored tests in `latch-engine`:

- `paced_socket_measurement` (`tests/support/lifecycle.rs`, compiled into
  the library's tests) drives the production
  pacer, packetizer, batch sender, controller and reassembler with
  synthetic encoded frames at 60 fps for three seconds, through real UDP
  sockets and a rate-limited FIFO with a 50 ms queue. It isolates the
  sender: there is no codec, and feedback is sampled in-process.
- `noise_socket_measurement` (`tests/bottleneck.rs`) runs a whole session
  for five seconds: the 640×360 noise test pattern, software H.264 encode,
  a UDP relay with the same FIFO, and a hardware-decoding viewer. It counts
  the frames the viewer published and the gaps between them.

Three builds were compared, five alternating rounds each, and the tables
give the median of the five with the range in brackets:

- **before**: the sender as it was before paced batches (`69539f0`);
- **batches**: paced batches as first merged (`aec0dd5`);
- **backlog**: the same with the backpressure rule corrected (`ed32f66`,
  below).

The controller fix came after these three and is compared separately,
[further down](#the-pace-on-an-unmeasured-path).

Every run's output line is kept: [session.txt](session.txt) for the whole
session and [sender.txt](sender.txt) for the sender alone, where the three
builds are labelled `old`, `new` and `fix`.

```sh
cargo test --release -p latch-engine --lib paced_socket_measurement --no-run
cargo test --release -p latch-engine --test bottleneck --no-run
# Each prints the test executable it built.
PB_TEST_BIN=<lib executable> sh docs/measurements/2026-11-09-paced-batches/run.sh
sh docs/measurements/2026-11-09-paced-batches/noise.sh <bottleneck executable>
```

`PB_CAP` is the FIFO's rate in kbps (0 for none), `PB_RTT` the round trip
in ms, `PB_REQUEST` the requested bitrate in kbps (twice the cap when
unset).

## Whole session: frames the viewer showed in five seconds

300 is every frame. Gap is the time between published frames, p50/p95.

| Link (request) | before | batches | backlog |
|---|---|---|---|
| 5 Mbps, 20 ms (10) | 285 [285–288], 16.3/20.1 ms | 286 [280–288], 16.3/20.0 | 287 [284–290], 16.3/21.2 |
| 5 Mbps, 150 ms (10) | 278 [263–278], 16.3/21.2 | 278 [249–278], 16.4/21.1 | 278 [264–278], 16.4/20.1 |
| 20 Mbps, 20 ms (40) | 296 [296–297], 16.4/18.1 | 296 [296–297], 16.4/18.5 | 296 [296–297], 16.3/18.3 |
| 20 Mbps, 150 ms (40) | 299 [299–299], 16.3/18.6 | 299 [297–299], 16.4/18.5 | 299 [169–300], 16.3/18.7 |
| 50 Mbps, 20 ms (100) | 296 [278–298], 16.3/17.7 | 297 [243–297], 16.3/17.6 | 296 [270–297], 16.4/18.4 |
| 50 Mbps, 150 ms (100) | 299 [259–300], 16.4/18.6 | 299 [299–300], 16.4/18.5 | 299 [221–300], 16.4/18.9 |
| no limit (100) | 264 [259–281], 16.5/36.2 | **213** [213–263], 22.2/36.8 | 252 [249–262], 17.4/35.2 |
| no limit (1000) | 300 [300–300], 16.4/18.8 | 300 [300–300], 16.3/18.4 | 300 [300–300], 16.3/18.5 |

The "no limit (100)" row is the one that moved. Paced batches as first
merged waited for the sender to be completely idle before taking the next
picture, so a frame cost its send time plus its encode time, and a fifth
of the frames went missing. Letting one frame be encoded while the
previous one is sent (`PACED_BACKLOG` in `src/server.rs`) brought it most
of the way back: 252 against the old sender's 264. The medians of the
other rows do not differ.

Two things in this table were first read wrongly. The no-limit row was put
down to the software encoder being the slow part, and the low runs at 150
ms (169 of 300 at 20 Mbps, 221 at 50 Mbps) to a loaded machine. Both were
the controller, and neither was new with paced batches.

## The pace on an unmeasured path

A stream that sends less than half its budget counts as idle, even when
it is moving: this test pattern is about 9 Mbps of video, 12 with parity,
under budgets of 40 and 100 Mbps. An idle stream is probed with short
trains of packets so that the controller knows what the path carries
before motion fills it. When two trains had gone out and no measurement
had come back, the controller fell back to pacing at a twentieth of the
budget, whatever the stream was already delivering.

That fallback was reached in two ways:

- **A long round trip.** The second train goes out 200 ms after the
  first. On a 150 ms round trip the first train's answer arrives at about
  the same moment, and since paced batches a train is also cancelled when
  a frame is waiting. Either way the count reached two with nothing
  measured, and a stream delivering 12 Mbps was paced at 2. In fourteen
  traced runs at 20 Mbps and 150 ms, three collapsed to about 12 frames a
  second and took four seconds to climb back ([collapse.txt](collapse.txt)
  has a good and a bad run side by side). The build as first merged did
  the same in 3 runs of 20 (their output was not kept).
- **A path too fast to measure.** With no rate limit and no delay the
  trains gave no usable measurement at all, so the stream was paced at 5
  Mbps (a twentieth of 100) for the whole session and its pace only crept
  up to about what it was sending. That is the 264 of 300, with 34 ms
  between frames at p95, that the "no limit (100)" row shows for every
  build, the oldest included.

The fallback is now the larger of a twentieth of the budget and twice the
best rate the viewer reported receiving in the last second; it rises as
delivery does, and the first train measured after it takes its place
(before, a higher measurement could not replace a lower guess). A still
desktop, which delivers almost nothing, gets the same cautious pace as
before.

Ten alternating rounds, the sender before paced batches against the
current build (`12a1598`); every line is in
[controller.txt](controller.txt). Median frames of 300, the range, and
the median gap between frames (p50/p95):

| Link (request) | before | now |
|---|---|---|
| 5 Mbps, 20 ms (10) | 286 [281–289], 16.5/20.6 ms | 285.5 [272–288], 16.6/21.0 |
| 5 Mbps, 150 ms (10) | 264 [234–278], 16.5/20.9 | 267.5 [234–279], 16.5/21.4 |
| 20 Mbps, 20 ms (40) | 297 [258–297], 16.4/18.6 | 296.5 [294–297], 16.4/18.6 |
| 20 Mbps, 150 ms (40) | 299 [247–299], 16.4/18.8 | 299 [299–299], 16.4/18.5 |
| 50 Mbps, 20 ms (100) | 297 [296–297], 16.4/17.7 | 297 [296–298], 16.4/17.7 |
| 50 Mbps, 150 ms (100) | 299 [262–300], 16.4/19.0 | 299 [299–300], 16.4/17.7 |
| no limit (100) | 265 [260–279], 16.6/33.8 | **300** [300–300], 16.4/19.0 |
| no limit (1000) | 300 [300–300], 16.4/18.8 | 300 [300–300], 16.5/19.0 |

The no-limit row now delivers every frame, and no run of the current
build at 20 or 50 Mbps fell below 294. A further 25 traced runs at 20
Mbps and 150 ms all completed (the last line of `collapse.txt`). The old
sender has low runs of its own in three rows here; it was not immune,
only less exposed. The 5 Mbps rows are unchanged: there the link, not the
pace, is what limits the stream, and at 150 ms both builds lose 20 to 65
frames while the controller finds the rate.

One more rule came after that table: a pace taken from a measured train
is not set under 125% of what the viewer is receiving, since a train can
read low. It was spot-checked with eight runs in five of the rows, not
the full comparison: 299 in every run at 20 and at 50 Mbps with 150 ms,
300 or 301 with no limit, 296 or 297 at 20 Mbps and 20 ms, and 286 to 289
at 5 Mbps and 20 ms.

The sender-alone table below was not repeated with these changes.

## Sender alone: three seconds of synthetic frames

Goodput is payload in frames the reassembler completed. Queue is the p95
age of a frame when the sender finished it. Lost is frames the reassembler
gave up on (the first frame is deliberately larger than the FIFO).

| Link (request) | before | batches | backlog |
|---|---|---|---|
| 5 Mbps, 20 ms (10) | 1.8 Mbps, 2.2 ms, 3 lost | 1.6, 2.8, 3 | 1.5, 2.0, 3 |
| 5 Mbps, 150 ms (10) | 1.0, 0.1, 6 | 0.7, 0.0, 6 | 1.0, 0.0, 6 |
| 20 Mbps, 20 ms (40) | 8.8, 4.9, 3 | 9.3, 5.2, 3 | 9.4, 5.0, 3 |
| 20 Mbps, 150 ms (40) | 6.1, 4.9, 6 | 6.1, 4.7, 6 | 6.2, 4.9, 6 |
| 50 Mbps, 20 ms (100) | 24.8, 5.2, 3 | 26.9, 5.7, 3 | 25.4, 5.6, 3 |
| 50 Mbps, 150 ms (100) | 16.2, 5.2, 7 | 16.8, 5.2, 6 | 14.9, 5.3, 6 |
| no limit (100) | 82.4, 1.0, 0 | 82.4, 1.0, 0 | 82.4, 1.0, 0 |
| no limit (1000) | 737.7, 9.3, 8 | 737.8, 9.0, 8 | 734.3, 8.8, 9 |

Through this harness the three senders are the same within noise. The
harness has its own loop standing in for capture, and for these runs that
loop did not use the corrected rule in any of the three builds, so this
table compares the pacer and batch sender only; it says nothing about the
backpressure rule. (The harness now follows `PACED_BACKLOG`.) On a Mac
that is the expected result: macOS has no batched UDP send, so the batch
path still makes one system call per datagram. The 1 Gbps row tops out near
740 Mbps and loses frames in all three. The test's loopback relay is the
likely limit, though it was not measured on its own; either way this is
not a clean measurement of the sender.

## Timers

1000 `thread::sleep` calls per duration on the same Mac (`sleep.rs`),
requested against actual, in microseconds:

| Requested | p50 | p95 | max |
|---:|---:|---:|---:|
| 10 | 15.2 | 19.2 | 46.4 |
| 100 | 129.0 | 140.0 | 429.6 |
| 500 | 630.8 | 641.0 | 831.2 |
| 1000 | 1257.4 | 1269.0 | 1610.2 |

Sleeps overshoot by about a quarter. The pacer sleeps to absolute deadlines
and does not spin; nothing above justified spending a core on it.

## Not measured

- **Content that changes rate.** The test pattern is a steady 9 Mbps. A
  scene change while the pace is a guess ramps by doubling, one step per
  report after a round trip, and a steady stream under a guessed pace is
  spread over up to half its frame interval. Neither was timed.

- **Linux GSO and Windows USO on real hardware.** The point of batching is
  fewer system calls on those two systems. The code paths are exercised by
  CI, but their effect on CPU and throughput has not been measured on a
  network card.
- **Latency of each frame from capture to display.** These runs count
  frames and the gaps between them; they do not time a frame end to end.
- **A clean path above about 700 Mbps.** The loopback relay loses frames
  first.
- **Timer behaviour on Windows.**
