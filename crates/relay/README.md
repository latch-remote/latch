# latch-relay

Native public TCP/TLS transport for Latch. Both peers connect outbound to a
configured public hostname, normally port 443. ALPN is `latch-relay/1`.
This requires a network that permits native TLS with this ALPN.

## Integration contract

1. Check `ensure_supported_cpu()` before enabling relay. Every crate entry into
   Graviola also checks this. Unsupported CPUs return `io::ErrorKind::Unsupported`.
2. Construct `Endpoint::new(hostname, port)` and `ClientConfig::production(endpoint)`.
   Production uses standard WebPKI roots, hostname verification, and rejects
   private, loopback and tailnet DNS results. Explicit
   `ClientConfig::with_roots` supports private PKI and loopback test certificates.
3. Host calls `create_grant(&config, &cancel)` then `SessionKeys::generate()`.
   Send only `grant.viewer` and the fresh keys to the authorized viewer through
   existing Tailscale-authenticated control. Keep `grant.host` on the host.
   Attachment and keys implement serde for this existing control channel.
   `Attachment` exposes `role()`, `expires_unix`, `session_seconds` and
   `session_bytes`. Its rendezvous ID and independent per-lane tickets travel
   inside the serialized capability.
4. Each side calls `Transport::connect(config, attachment, keys)` which starts two
   cancellable workers, one TLS owner per lane. Calls return immediately.
   Await `Event::Ready` before submitting traffic. Ready requires authenticated
   key confirmation on both lanes. A new attempt needs a new grant and new keys.
5. `try_send_datagram(&[u8])` accepts existing priority wire packets up to 1200
   bytes. `try_send_control(&[u8])` sends authenticated serialized control, up to
   16,359 bytes (16 KiB less the counter, AEAD tag and type byte).
   `try_send_frame(&InFrame)` queues the complete encoded frame including frame number, keyframe/recovery flags and host delay.
   Input remains owned by the caller. `WouldBlock` means backpressure. Stop the
   session and release held inputs if priority traffic cannot be delivered.
   Callers may discard a rejected whole frame and force a keyframe.
6. `recv_timeout(Duration)` returns `Ready`, `Datagram`, `Control`, complete
   `Frame`, or `Closed`. Complete frames bypass UDP assembly and FEC entirely.
   `shutdown()` and Drop cancel and join workers. Transport is thread-safe.

`ClientConfig::limits` bounds queue bytes, maximum accepted frame size, setup,
frame/send stall and idle deadlines. Defaults permit 8 MiB frames with one
outgoing frame queued, one outgoing in flight, one incoming incomplete frame,
and byte-bounded delivered events. Increase the frame bound up to
`latch_proto::video::MAX_FRAME_LEN` when needed. Exhausting incoming event space
closes the whole attempt, including priority. A partially sent frame is never
discarded or interleaved. `discard_queued_frame()` removes only the unsent frame.
Both endpoints acknowledge complete video frames inside E2E encryption. A
256-frame acknowledgment metadata window and a bounded return-path deadline
limit stalls after TCP accepts bytes. ACK deadlines use that video lane's
smoothed RTT plus four times its variation, with the configured stall deadline
as a floor and a five-second ceiling. Sender acknowledgment metadata is fixed-size.
`Closed` invalidates queued events and is terminal. Release held inputs on every
close or priority send failure. Successful `try_send_*` means accepted locally.

Default client limits are 256 KiB queued priority bytes, 8 MiB queued video data,
9 MiB delivered event bytes, 8 MiB frame size, 10 s setup, 750 ms frame/send
deadline and ACK floor, and 30 s peer idle timeout. The outgoing queue holds one whole frame and
one additional frame may be in flight. Frame deadlines include queue residence.
Setup, stall and idle configuration ceilings are 60 s, 5 s and 300 s.
Keepalives are encrypted and automatic. Control shares the priority lane.
`InvalidData` identifies invalid payload sizes, `NotConnected` precedes Ready,
`WouldBlock` identifies byte/whole-frame backpressure and `BrokenPipe` follows
closure. `recv_timeout` returns `None` on timeout and after Closed is consumed.

OS DNS runs on one process-wide worker with a 16-request queue. The caller can
cancel while DNS is blocked. The worker can remain inside the OS resolver until
that OS call returns. Each transport has exactly two joinable network workers.
TCP connect, TLS handshake and all subsequent I/O are nonblocking and cancellable.

```rust
use latch_relay::{create_grant, Cancel, ClientConfig, Endpoint, SessionKeys, Transport};

let config = ClientConfig::production(Endpoint::new("relay.example.com", 443)?)?;
let grant = create_grant(&config, &Cancel::new())?;
let keys = SessionKeys::generate()?;
let viewer_capability = grant.viewer;
let host = Transport::connect(config, grant.host, keys.clone())?;
// Deliver viewer_capability and keys through existing authorized control.
// The viewer constructs its own ClientConfig and calls Transport::connect.
```

Application wiring and automatic selection between direct UDP and relay belong
to the engine/stream integration, which configures the public endpoint explicitly.

## Cryptography and CPU support

Graviola 0.4.2 ChaCha20-Poly1305 protects every application record. Four independent
random 256-bit base keys cover role direction and lane. RFC 5869 HKDF-SHA256
using Graviola derives each traffic key with rendezvous ID salt and the complete
authenticated context as expansion info. Version, random rendezvous ID,
fresh generation, lane, direction and strict counter are authenticated. Each
nonce is a zero prefix and a monotonically increasing 64-bit counter. Confirmation
is the first encrypted record in each direction. The relay receives capabilities
and opaque ciphertext. Session keys remain on the endpoints. Debug output redacts capabilities
and keys. Reusing attachment/keys across attempts is forbidden.

Graviola's actual mandatory CPU gates require AES, PCLMULQDQ, BMI1, ADX, AVX and
AVX2 on x86-64, and NEON, AES, PMULL and SHA2 on AArch64. Relay configuration,
key generation, encryption and daemon setup check these before entering crypto.
Debug builds honor Graviola's `GRAVIOLA_CPU_DISABLE_<feature>` switches too.
The public endpoint operator must select a supported CPU. The library targets
these two architectures across platforms. The daemon supports Linux and macOS.

## Daemon

The daemon exposes create and fixed rendezvous attach. Forwarding joins paired
inbound sockets using the server-issued capabilities. Run
`cargo run -p latch-relay -- --help` for explicit operator configuration. TLS certificate/key are PEM files.
The quota file must be created explicitly using `--initialize-quota` on first
startup, and then retained on persistent storage across restarts.

`DaemonConfig::new(listen, certificate_chain, private_key, quota_path,
monthly_bytes)` builds a verified TLS identity with Graviola. Set
`initialize_quota = true` only for first initialization. Set `config.limits` for
operator overrides, then call `Daemon::bind(config)`. `local_addr()` supports
loopback port-zero tests, `metrics()` returns fixed atomic counters, and
`run(&Cancel)` owns the event loop until cancellation. The daemon adds no threads
per connection. The CLI prints metrics on stdout every 30 s.

### Pilot defaults

| Limit | Default |
| --- | --- |
| Live connections, global / IP | 24 / 8 |
| Live grants or sessions, global / creator IP | 4 / 2 |
| Accepted TCP connections per minute, global / IP | 120 / 30 |
| Rate metadata entries | 256 |
| Grant attach TTL | 60 s |
| Session lifetime / encrypted TCP byte cap | 900 s / 512 MiB |
| TLS/request/pairing setup deadline | 10 s |
| Pending record deadline / idle timeout | 750 ms / 30 s |
| TLS handshake byte limit per connection | 128 KiB |
| Encrypted application record / forwarding buffer | 16 KiB |
| CLI monthly ingress plus egress reservation budget | 10 GiB |

Admission is an anonymous bounded pilot with separate random server-issued host
and viewer capabilities. Each role carries independent single-use priority and
video tickets, bound to that role and lane.
Invalid or duplicate attach requests fail. Losing any attached lane closes the
entire session. Grant metadata occupies fixed session slots. Certificates,
configuration files, handshake work, record lengths and rate metadata are bounded.
TLS session caches and resumption tickets are disabled on the daemon.

`--limits-json` accepts a partial JSON object with `PilotLimits` field names.
Unknown fields fail. Durations use serde's `{"secs": 60, "nanos": 0}` representation.
For example, `{"max_sessions": 2, "sessions_per_ip": 1}` reduces admission.
Overrides have finite ceilings, including 512 connections, 64 sessions, 4096 rate
entries, 300 s grant TTL and 24 h session lifetime. The transfer budget remains
mandatory. The default bind is loopback `127.0.0.1:8443`. A public bind is explicit.

```sh
cargo build --release -p latch-relay
# First start after the operator provides a public certificate and private key.
target/release/latch-relay --listen 0.0.0.0:443 \
  --cert /etc/latch/relay-chain.pem --key /etc/latch/relay-key.pem \
  --quota-file /var/lib/latch/relay.quota --monthly-bytes 10737418240 \
  --initialize-quota
# Subsequent starts use the same arguments with --initialize-quota omitted.
```

### Durable accounting

Every socket reserves 64 KiB byte leases in a locked 32-byte ledger and fsyncs
before reading or writing TLS bytes. Each lease covers encrypted TCP payload
bytes in both directions, including handshakes, capabilities and forwarding.
Unused credit remains charged after disconnect or restart. Missing, truncated,
corrupt or concurrently locked ledgers fail closed. IO errors are sticky and
stop the daemon. Initialization uses exclusive creation. Retain the ledger on
persistent storage. Deleting or rolling back that operator-owned file erases its
history.

UTC calendar-month indices only advance. A backward clock keeps charging the
latest recorded month. Advancing the month invalidates outstanding leases on
existing sockets before more IO. Session byte limits reserve room for the next
bounded IO turn. A record is forwarded only when its peer has drained the prior
record. Congestion stays in bounded TLS and OS socket buffers, with peer and
frame acknowledgment deadlines.

`network_bytes` counts observed encrypted TCP payload ingress plus egress.
`quota_reserved_bytes` includes unused reserved credit. Cloud billing also
includes kernel IP/TCP headers, retransmissions, rejected connection traffic and
other services on the instance. Cloud billing validation and budget headroom
remain operator deployment work.

## Runnable checks

```sh
cargo test -p latch-relay
cargo clippy -p latch-relay --all-targets -- -D warnings
cargo run -p latch-relay -- --help
```

Tests use real loopback TLS with an explicit test CA. They cover complete video,
authenticated control and priority traffic, confirmation with wrong keys,
capability replay, hostname and trust failures, AEAD tamper/replay/session/lane/
direction binding, malformed/truncated records and frame headers, byte queue
backpressure, partial TLS writes, stalled/closed sockets, cancellation/drop, CPU
rejection in a separate process, durable quota restart, locking, backward month
handling and sticky persistence failures. The published localhost private key
under `tests/certs` is a test fixture. Regenerate fixtures with
`sh crates/relay/tests/certs/generate.sh` when their ten-year certificates expire.

The 23 tests execute on macOS AArch64, and the release daemon builds locally.
All-target type checks also pass for Linux x86-64, Linux AArch64 and Windows
x86-64 MSVC. Native execution on Linux, Windows and x86-64 hardware remains
integration/CI validation work.
