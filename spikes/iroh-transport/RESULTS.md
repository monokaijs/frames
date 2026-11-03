# Spike 0-A: iroh as the Frames v2 transport

The plan is in `~/.claude/plans/plan-on-a-project-lazy-candy.md`, Phase 0. Versions tested: iroh 1.4.1, noq / noq-proto / noq-udp 1.4.0 (n0's multipath fork of quinn). This crate is standalone and is not a member of the Frames workspace.

```sh
cargo build --release
./target/release/iroh-transport-spike host [--relay-only] [--cc target|cubic] [--rates 10,40,80,10] [--step-secs 4]
./target/release/iroh-transport-spike connect <endpoint-id> [--relay-only] [--secs 17]
```

## Answers

| Question | Result |
|---|---|
| Custom congestion controller through iroh | **Yes.** `QuicTransportConfig::builder().congestion_controller_factory(..)` takes a `noq_proto::congestion::ControllerFactory`. A controller can return `ControllerMetrics.pacing_rate`, and noq's pacer honours it (`noq-proto/src/connection/pacing.rs:97`). The `TargetRate` controller here followed 10→40→80→10 Mbps exactly. There is one controller per path, so relay and direct paths each get their own. |
| Datagrams | **Yes.** `send_datagram`, `read_datagram` and `max_datagram_size` exist. The size is 1162 B before path MTU discovery or on the relay path, and 1200 B once direct. Size packets from `max_datagram_size()` and never hard-code it. |
| `export_keying_material` | **Yes.** Both ends printed the same 16-byte exporter. Pairing codes and the `LegacyProof` identity migration can stay as designed. |
| Path visibility | **Yes.** `Connection::paths()` gives `is_relay`/`is_ip`/`is_selected`/`rtt`/`remote_addr` per path, and `path_events()` reports changes. That is enough for a "Relayed" indicator and per-path policy. |
| Relay-only test mode | **Yes.** `Builder::clear_ip_transports()`. |
| Hole punching / upgrade | Connected over the relay in about 500 ms, then upgraded to direct within about 1 s on the same host. The upgrade second showed 2.5% loss and p99 queueing of 59 ms; the media engine must absorb the path switch (keyframe-free recovery, reset the delay baseline). |
| macOS reply source address (why we vendor quinn-udp) | **Bug still present in noq-udp 1.4.0.** IPv4 source is encoded with `IP_RECVDSTADDR` in both `unix.rs:712` and `apple_fast.rs:114` (fast datapath is a default feature). macOS ignores that on send. **Port `vendor/quinn-udp/FRAMES-PATCH.md` to `noq-udp` (both files)** and offer it upstream to n0. Cross-machine confirmation: see below. |
| DSCP / QoS marking | **Not exposed.** There is no raw socket access (`bound_sockets()` returns addresses only). noq-udp already writes an `IP_TOS` cmsg for ECN per packet (`unix.rs:676`, `apple_fast.rs:101`), so on macOS/Linux DSCP can be ORed into that byte in the same vendored patch. Windows needs qWAVE `QOSAddSocketToFlow` on the socket handle, which means a small hook in the vendored `noq-udp/src/windows.rs`. |

## Measurements (Mac, both ends on one host, 2026-11-04)

**Direct path** (selected 192.168.0.131, RTT 0.5 ms), `TargetRate` controller:

| Target | Received | Loss | Queueing p50 / p95 / p99 |
|---|---|---|---|
| 10 Mbps | 10.0 | 0% | 0.3 / 0.5 / 0.8 ms |
| 40 Mbps | 40.0 | 0% | 0.5 / 0.9 / 2.2 ms |
| 80 Mbps | 79.9 | 0–1.1% | 0.5 / 1.4–2.4 / 3–11 ms |

**Relay only** (n0 public relay aps1-1, RTT 75 ms):

| Target | Received | Loss | Queueing p50 / p95 |
|---|---|---|---|
| 5 Mbps | 5.0 | 0% | 0.6 / 1.5 ms |
| 10 Mbps | 10.0 | 0% | 0.5 / 1.5 ms |
| 20 Mbps | **7–10** | **6–61%** | **145 → 1150 / 500 → 1210 ms** |

## Cross-machine: Windows client → Mac host (same LAN, 2026-11-04)

The Mac has two LAN addresses: en0 192.168.0.169 (default route) and en1 192.168.0.131. The Windows client is 192.168.0.100. `--direct` dials one address with relays and address lookup disabled.

| Test | Result |
|---|---|
| `--direct 192.168.0.169` (default interface) | **Works.** Connected in 4 ms. 20/60 Mbps delivered; queueing p95 0.6–4.4 ms; 0–0.9% loss; exporters match. |
| `--direct 192.168.0.131` (secondary interface) | **Broken, half-open.** The client reports "connected" on a path to **.169**: the Mac replied from the wrong source address. The Mac host never completes the accept, no data flows, and the host's accept later fails with "timed out". **This confirms the noq-udp macOS source-address bug, and iroh turns it into a silent half-open connection instead of a clean handshake failure.** |
| Normal iroh dialing (discovery + relay) | Connected through the relay in 509 ms, but **did not upgrade to direct within 8 s**, although both machines are on the same LAN. On the relay at a 20 Mbps target, queueing grew to 1.7 s. **Cause not yet determined:** Windows Firewall silently dropping inbound probes (the exe was started over SSH, so no Allow prompt), and/or the source-address bug breaking path validation. Next step: add a firewall rule for the spike exe, run 30 s or more, and repeat after patching noq-udp. |

### After porting the source-address fix to noq-udp (`vendor/noq-udp`, see its FRAMES-PATCH.md)

| Test | Result |
|---|---|
| Windows Firewall | Not involved. The active profile is Public, and its firewall state is OFF (read-only `netsh advfirewall show`). |
| `--direct 192.168.0.131` (secondary) | The handshake now completes correctly: the host accepts on .131. Then multipath opens a second direct path to .169 and switches to it, and host→client delivery drops 68–99%. |
| Normal dialing | LAN peers **now upgrade from the relay to direct**; unpatched, they never did. But host→client data stops right after the upgrade (≥99% loss). The same happens with the `fast-apple-datapath` feature off, so it is not the `sendmsg_x` batch path. |
| Host trace (`RUST_LOG=noq_proto::connection=debug,iroh::socket=debug`) | `path added validated=false path_id=1 network_path=(local: 192.168.0.169, remote: [::ffff:192.168.0.100]:…)`, then the remote marks it `Backup`. **The host never validates the direct path, while the client selects it.** The direct path uses the **IPv6 dual-stack socket with an IPv4-mapped address**. The IPv4 `IP_PKTINFO` fix does not cover that case; the IPv6 branch sends `IPV6_PKTINFO` with a mapped address, which macOS may not honour for source selection. |

**Status: unresolved and blocking for Phase 4.** On this dual-interface, same-subnet Mac, which is common when Wi-Fi and Ethernet are both connected, iroh multipath loses host→client traffic after the relay→direct upgrade. Next steps:
- Run a control test with one interface disabled. That needs the user, because it changes the network config.
- Extend the patch so IPv4-mapped sources on the v6 socket use `IP_PKTINFO`, or bind IPv4 separately (iroh binds both families).
- Reproduce with plain noq to confirm the cause, then report to n0 with this repro.
- Until fixed, the Frames v2 connection manager needs a guard that falls back to the relay, or pins the path, when a selected direct path stops delivering. The media engine sees this within ~100 ms through transport feedback.

## Consequences for the plan

1. **n0's public relays deliver about 8–10 Mbps per connection.** That is not enough for 1080p game streaming. Bring the self-hosted `iroh-relay` forward from "before GA" to the start of the Phase 4 beta. Relay mode must cap video at about 6 Mbps until we run our own relays, and measure the real cap with probing.
2. **QUIC's RTT stat did not move (74.7 ms) while real queueing reached 1.2 s.** Rate control must not rely on QUIC RTT. It needs application one-way-delay measurement from per-packet send timestamps, which the Phase 1 wire format already adds. Here the delay signal rose from about 1 ms to 145 ms within one second, early enough for a delay-gradient controller to back off before loss.
3. The target-rate controller ignores loss by design. The media rate controller must therefore own loss and delay reaction completely, including on relay paths, or a misbehaving controller floods the path, as at 20 Mbps above.
4. Vendoring moves from `quinn-udp` to `noq-udp` (source-address fix, plus DSCP).
5. The multipath and dual-stack path validation issue above must be fixed or worked around before Phase 4 can pass its NAT matrix gate. Add a dual-interface Mac to that matrix.
