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
- **now**: the same with the backpressure rule corrected (below).

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

| Link (request) | before | batches | now |
|---|---|---|---|
| 5 Mbps, 20 ms (10) | 285 [285–288], 16.3/20.1 ms | 286 [280–288], 16.3/20.0 | 287 [284–290], 16.3/21.2 |
| 5 Mbps, 150 ms (10) | 278 [263–278], 16.3/21.2 | 278 [249–278], 16.4/21.1 | 278 [264–278], 16.4/20.1 |
| 20 Mbps, 20 ms (40) | 296 [296–297], 16.4/18.1 | 296 [296–297], 16.4/18.5 | 296 [296–297], 16.3/18.3 |
| 20 Mbps, 150 ms (40) | 299 [299–299], 16.3/18.6 | 299 [297–299], 16.4/18.5 | 299 [169–300], 16.3/18.7 |
| 50 Mbps, 20 ms (100) | 296 [278–298], 16.3/17.7 | 297 [243–297], 16.3/17.6 | 296 [270–297], 16.4/18.4 |
| 50 Mbps, 150 ms (100) | 299 [259–300], 16.4/18.6 | 299 [299–300], 16.4/18.5 | 299 [221–300], 16.4/18.9 |
| no limit (100) | 264 [259–281], 16.5/36.2 | **213** [213–263], 22.2/36.8 | 252 [249–262], 17.4/35.2 |
| no limit (1000) | 300 [300–300], 16.4/18.8 | 300 [300–300], 16.3/18.4 | 300 [300–300], 16.3/18.5 |

The "no limit (100)" row is the one that moved. In that case the software
encoder on this machine is the slow part, and the controller turns pacing
on. Paced batches as first merged waited for the sender to be completely
idle before taking the next picture, so a frame cost its send time plus
its encode time, and a fifth of the frames went missing. Letting one frame
be encoded while the previous one is sent (`PACED_BACKLOG` in
`src/server.rs`) brings it back to within the spread of the old sender.
The other rows do not differ beyond their run-to-run spread; every build
has an occasional low run on this loaded machine.

## Sender alone: three seconds of synthetic frames

Goodput is payload in frames the reassembler completed. Queue is the p95
age of a frame when the sender finished it. Lost is frames the reassembler
gave up on (the first frame is deliberately larger than the FIFO).

| Link (request) | before | batches | now |
|---|---|---|---|
| 5 Mbps, 20 ms (10) | 1.8 Mbps, 2.2 ms, 3 lost | 1.6, 2.8, 3 | 1.5, 2.0, 3 |
| 5 Mbps, 150 ms (10) | 1.0, 0.1, 6 | 0.7, 0.0, 6 | 1.0, 0.0, 6 |
| 20 Mbps, 20 ms (40) | 8.8, 4.9, 3 | 9.3, 5.2, 3 | 9.4, 5.0, 3 |
| 20 Mbps, 150 ms (40) | 6.1, 4.9, 6 | 6.1, 4.7, 6 | 6.2, 4.9, 6 |
| 50 Mbps, 20 ms (100) | 24.8, 5.2, 3 | 26.9, 5.7, 3 | 25.4, 5.6, 3 |
| 50 Mbps, 150 ms (100) | 16.2, 5.2, 7 | 16.8, 5.2, 6 | 14.9, 5.3, 6 |
| no limit (100) | 82.4, 1.0, 0 | 82.4, 1.0, 0 | 82.4, 1.0, 0 |
| no limit (1000) | 737.7, 9.3, 8 | 737.8, 9.0, 8 | 734.3, 8.8, 9 |

Through this harness the three senders are the same within noise. On a Mac
that is the expected result: macOS has no batched UDP send, so the batch
path still makes one system call per datagram. The 1 Gbps row tops out near
740 Mbps and loses frames in all three; the loopback relay is the limit
there, not a clean measurement of the sender.

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

- **Linux GSO and Windows USO on real hardware.** The point of batching is
  fewer system calls on those two systems. The code paths are exercised by
  CI, but their effect on CPU and throughput has not been measured on a
  network card.
- **Latency of each frame from capture to display.** These runs count
  frames and the gaps between them; they do not time a frame end to end.
- **A clean path above about 700 Mbps.** The loopback relay loses frames
  first.
- **Timer behaviour on Windows.**
