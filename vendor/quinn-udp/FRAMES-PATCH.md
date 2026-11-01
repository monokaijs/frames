# Frames patch to quinn-udp 0.5.16

Source: https://crates.io/crates/quinn-udp/0.5.16 (MIT OR Apache-2.0).

On Apple platforms, encode outgoing IPv4 source addresses with `IP_PKTINFO`
and `in_pktinfo`, as on Linux. macOS accepts `IP_RECVDSTADDR` on a send but
ignores it for source selection. A host listening on `0.0.0.0` consequently
replies from the default interface instead of the interface contacted by the
client. QUIC handshakes to a secondary Wi-Fi/Ethernet address time out.

Verified on a Mac mini with Ethernet and Wi-Fi on the same LAN: the old
control message sent a requested Wi-Fi source from the Ethernet address;
`IP_PKTINFO` preserved the requested source. Keep the existing receive path,
ECN handling, batching, and platform implementations intact.

Remove this patch when the upstream dependency includes the equivalent fix.
