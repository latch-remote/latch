# latch-relay

Public TCP/TLS transport for Latch. Both peers connect outbound to a public
hostname, normally port 443. Native ALPN `latch-relay/1` is attempted first.
A bounded fallback uses RFC 6455 WebSocket over HTTPS at `/relay` on the same
port, offering `h2, http/1.1` and normal relay SNI. The daemon selects HTTP/1.1.
Its ordinary HTTPS response is `200 OK` with a small text body.

## Integration contract

1. Check `ensure_supported_cpu()` before enabling relay. Every crate entry into
   Graviola also checks this. Unsupported CPUs return `io::ErrorKind::Unsupported`.
2. Construct `Endpoint::new(hostname, port)` and `ClientConfig::production(endpoint)`.
   Production uses standard WebPKI roots, hostname verification, and rejects
   private, loopback and tailnet DNS results. Explicit
   `ClientConfig::with_roots` supports private PKI and loopback test certificates.
3. Host calls `create_grant_with_info(&config, &cancel)` then
   `SessionKeys::generate()`. The result includes typed carrier diagnostics.
   The existing `create_grant` entry point returns the grant alone.
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

Engine/stream integration configures the public endpoint and selects between
direct UDP and relay. The next wiring change can supply proxy settings and use
the diagnostics below.

## Carrier and proxy API

Set `config.proxy = Some(ProxyConfig::new(host, port)?)` for an explicit HTTP
CONNECT proxy. Its endpoint accepts DNS names, IPv4 and unbracketed IPv6 literals.
Set `proxy.credentials = Some(BasicCredentials::new(username, password)?)` for
Basic authentication. All credential Debug output is redacted. The proxy
connection uses plaintext HTTP. Relay TLS inside the tunnel verifies the relay
hostname against the configured roots. Production uses public WebPKI roots.
The caller supplies system/PAC discovery results.

An explicit proxy is authoritative for both carriers. A cold connection has a
two-second ladder, with at most 750 ms for native DNS/TCP/TLS. WebSocket gets
the remaining time, including at least 1.25 s after a full native timeout.
Cloned configurations remember the winning carrier and TCP address for 30
seconds, scoped to relay/proxy endpoints and the locally selected source
address. A local UDP route probe sends no packets and invalidates the shortcut
when the source changes. Route changes retaining the same source are detected by
a failed connection attempt. Concurrent lanes share initial fallback selection.

Cached native probes get 750 ms and cached WebSocket probes get 1.25 s. Failure
clears the shortcut and restarts both cold rungs with fresh DNS and their full
two-second budget. An open therefore takes at most 2 s cold, 750 ms cached
native, 1.25 s cached WebSocket, 2.75 s after stale native or 3.25 s after stale
WebSocket. The caller's earlier deadline bounds every probe and fallback.
Refusal, reset, timeout, ALPN and upgrade rejection advance the ladder.
Untrusted-issuer, portal and proxy failures finish immediately.

With stable caching and 200 ms RTT, cold grant open plus concurrent warm attach
opens and four request/confirmation/control exchanges total at most 2 s + 1.25 s
+ 0.8 s = 4.05 s. Fresh attach configurations share the fallback selection and
total 2 s + 2 s + 0.8 s = 4.8 s. After native consumes 750 ms, the remaining
1.25 s WebSocket budget supports roughly 400 ms RTT direct and 300 ms through
CONNECT, allowing little DNS or scheduling delay at that edge. Grant, attach and
four exchanges then take about 5.2–5.6 s, leaving about 0.4–0.8 s under the
application's six-second negotiation deadline. These estimates assume working
routes, TLS 1.3 and no failed-family stagger. DNS delays, TLS 1.2, a 250 ms
address-family stagger or a stale path consume further time. The application
must cancel at its six-second deadline. An earlier native failure leaves more
time for WebSocket. Budgets stay fixed. Tests independently delay both gateway
directions by 100 ms and exercise stale-native recovery at 140 ms each way (280
ms RTT). The ten-second default setup limit covers request/pairing and
confirmation.

`create_grant_with_info(&config, &cancel)` returns `(Grant, ConnectionInfo)`.
`Transport::connection_info(Lane)` returns the chosen path once its connection
is established. `ConnectionInfo` contains `carrier`, `via_proxy`, `reduced_mss`,
`reduced_mss_reason` and `earlier_failures`. `MssReason` identifies explicit,
write or partial-record stalls. The existing `TlsHandshakeStall` enum variant is
retained for source compatibility and is no longer emitted automatically. Each
`AttemptFailure` contains its carrier, proxy use and `Failure`.
`ConnectError::from_io(&error)` recovers typed failures from the existing
`io::Result` API. `Transport::failure()` retains the terminal typed failure
while `Event::Closed` keeps its existing shape.

`Failure` distinguishes connection blocked/disconnected, `ClosedBeforeHandshake`
with the message "blocked or relay busy", timeout, untrusted or incomplete TLS
chains with bounded, control/format-free issuer text, probable captive portal,
proxy authentication/refusal, ALPN/upgrade rejection, relay rejection, invalid
protocol, cancellation and unsupported CPU. A different certificate name or
plaintext HTTP before verified TLS produces a probable captive portal failure.
HTTP/HTML/redirect responses inside verified relay TLS produce `UpgradeRejected`
and permit another carrier attempt. The daemon sends `[0]` for request rejection
when existing TLS credit can deliver it. Exhausted pre-TLS admission closes
cheaply. This has the same observable result as a network-induced early close,
so `ClosedBeforeHandshake` preserves that ambiguity.

Production clients verify the relay hostname using public WebPKI roots. Private,
loopback, link-local, ULA and tailnet destinations remain filtered, including
100.64.0.0/10, fd7a:115c:a1e0::/48 and its 4via6 range. IPv4-mapped addresses
and the well-known `64:ff9b::/96` require a public embedded IPv4, independently
of relay A answers or discovery. The entire RFC 8215 local-use `64:ff9b:1::/48`
range is denied, including all subprefixes and suffixes.

RFC 7050 discovery is optional and runs only when a local route probe finds no
usable IPv4 source and relay answers include other public global IPv6 addresses.
IPv4-only and dual-stack routes issue no discovery. The dedicated bounded worker
runs independently of relay DNS, caches answers for five minutes and gives
callers at most 150 ms. A missing or unanswered `ipv4only.arpa` lookup never
rejects an address that independently passes public-address filtering.
Identified global network-specific /96 prefixes require a public embedded IPv4
and a zero reserved u octet. Identified shorter RFC 6052 prefixes require public
IPv4 under all six legal layouts. Discovery requires both 192.0.0.170 and .171
at one prefix length. Resolver assertions cannot authorize local-use, private or
tailnet addresses. Global network-specific translators remain a limit of this
filter. An undisclosed prefix is indistinguishable from native global IPv6, and
discovery cannot prove a translator's actual prefix/layout or bind it to a relay
A answer. Such global addresses retain ordinary IPv6 filtering when discovery is
absent. TLS hostname verification still applies after connecting.

IPv4 and IPv6 race with a 250 ms stagger and Windows socket readiness checks.
Explicit proxies may reside on local networks. The supplied systemd unit binds
`[::]:443` with IPv4-mapped connections enabled. Admission aggregates native
IPv6 by /64 and normalizes mapped IPv4 and well-known NAT64 /96 to IPv4.
Translators normally present a gateway IPv4. Other translated IPv6 sources share
a /64 bucket.

A silent drop after ClientHello is classified as blocked. Automatic 1200-byte
MSS hints require small-record receive progress followed by a stalled large
write or partial large record. Typed private timeout payloads carry this
evidence. Hints expire after 30 seconds and are shared by cloned configurations.
DNS, connect, handshake-only, idle, ACK and session-lifetime timeouts leave the
hint untouched. `reduce_mss_after_stall()` records an externally detected
path-MTU stall. Linux applies the clamp before connect and macOS before TLS
after connect. `ConnectionInfo::reduced_mss` and `reduced_mss_reason` report
application and cause. Windows uses OS TCP behavior. Server TCP MTU probing
covers its sending direction.

WebSocket binary messages contain one existing length-prefixed record, at most
16 KiB plus four bytes, including fragments. Masking, ping/pong and close follow
RFC 6455. HTTP headers are bounded to 8 KiB and 32 fields. Ordinary root GET/HEAD requests
receive 200, unknown paths 404, unsupported methods 405, invalid requests 400,
incomplete upgrades on `/relay` 426 with version 13, and excessive headers 431. Native and WebSocket
share byte quotas, session slots, bounded queues and deadlines. A peer can use
a different carrier from its partner.

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
per connection. An IPv6 wildcard listen address enables dual-stack IPv4/IPv6
on one socket. Admission keys aggregate native IPv6 by /64 and normalize
IPv4-mapped and well-known NAT64 /96 sources to their IPv4 address. Translators
normally present their gateway's IPv4 source. Unrecognized translated IPv6
sources share their gateway /64. The CLI prints metrics on stdout every 30 s,
including aggregate `http_responses`. Completed decoy responses consume no
per-event diagnostic lines.

### Pilot defaults

| Limit | Default |
| --- | --- |
| Live connections, global / source key | 24 / 8 |
| Live grants or sessions, global / creator source key | 4 / 2 |
| Accepted TCP connections per minute, global / source key | 120 / 30 |
| Rate metadata entries | 256 |
| Grant attach TTL | 60 s |
| Session lifetime / encrypted TCP byte cap | 900 s / 512 MiB |
| TLS/request/pairing setup deadline | 10 s |
| Pending record deadline / idle timeout | 750 ms / 30 s |
| TLS handshake byte limit per connection | 128 KiB |
| Encrypted application record / forwarding buffer | 16 KiB |
| CLI monthly ingress plus egress reservation budget | 10 GiB |

The rate table evicts its oldest window when full, allowing a new source to
compete for global capacity. Per-source rate refusals consume no global accept
credit. IPv6 address rotation within a /64 shares all connection, session and
rate limits. A /48 holder can cycle 65,536 /64 keys, evict per-key windows and
consume the global accept allowance. A campus sharing one /64 shares its eight
connections and two sessions. The global 120 accepts/minute, 24 connections,
four sessions and byte quotas still bound total daemon load.

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
target/release/latch-relay --listen '[::]:443' \
  --cert /etc/latch/relay-chain.pem --key /etc/latch/relay-key.pem \
  --quota-file /var/lib/latch/relay.quota --monthly-bytes 10737418240 \
  --initialize-quota
# Subsequent starts use the same arguments with --initialize-quota omitted.
```

### Durable accounting

Every socket obtains 64 KiB byte leases from a durably reserved pool backed by
a locked 32-byte ledger. A dedicated persistence worker fsyncs batches of up to
4 MiB before the network loop can use them. The shared pool and its pending
refill total at most 6 MiB. Each batch fits the unreserved monthly budget.
Connections awaiting credit perform no TLS I/O. Existing credited connections
keep running while a refill is pending. Each lease covers encrypted TCP payload
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

Carrier tests also cover real-TLS WebSocket media, HTTP-ALPN-only middleboxes,
a silent native handshake, CONNECT with and without Basic authentication, 407,
403 and 5xx responses, private-CA interception, plaintext and HTTPS redirects,
upgrade rejection, ordinary HTTPS responses, dual-stack admission, DNS64 address
filtering, fragmented/masked frame bounds and receive-queue parity. Run the suite
on each supported OS through CI.
