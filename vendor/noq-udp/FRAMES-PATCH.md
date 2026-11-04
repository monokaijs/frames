# Frames patch to noq-udp 1.4.0 (iroh 1.4.1's UDP layer)

Source: https://crates.io/crates/noq-udp/1.4.0 (MIT OR Apache-2.0).

Port of `vendor/quinn-udp/FRAMES-PATCH.md`. On Apple platforms, encode outgoing
IPv4 source addresses with `IP_PKTINFO`/`in_pktinfo` instead of
`IP_RECVDSTADDR`, in both send paths (`unix.rs` and the default
`fast-apple-datapath` `apple_fast.rs`). macOS ignores `IP_RECVDSTADDR` on send,
so a multihomed Mac bound to 0.0.0.0 replies from its default interface.
Under iroh this produced a half-open connection (client "connected", host never
accepted) when dialing the Mac's secondary LAN address.

Remove when upstream includes the equivalent fix.
