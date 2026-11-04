# Self-hosted relays for Frames v2

Frames v2 connects devices directly whenever it can (iroh hole punching). When
no direct path works (a firewall blocks UDP, or both sides sit behind strict
NATs), traffic goes through a relay. n0's public relays work for development,
but deliver only about 8–10 Mbps per connection, so Frames caps streams on
them at 6 Mbps (`frames_net::N0_RELAY_MAX_BPS`). Self-hosted relays have no
such cap: through a relay on the LAN, `frames-next` streamed 1080p60 at
36 Mbps with capture → present p50 4.3 ms.

The relay is n0's `iroh-relay`, vendored in `vendor/iroh-relay` (only change:
no cdylib, see `FRAMES-PATCH.md`).

## Build

```sh
cargo build --release --manifest-path vendor/iroh-relay/Cargo.toml \
    --features server --bin iroh-relay
```

## Try it locally

```sh
iroh-relay --dev          # HTTP on [::]:3340, no TLS
frames-next host --relay http://[::1]:3340 --relay-only ...
frames-next connect <id> --relay http://[::1]:3340 --relay-only
```

On Windows the dev server listens on IPv6 only; use `[::1]`, not `127.0.0.1`.

## Production

Run 2–3 relays, near the users (for example one each in North America,
Europe, and Asia), each on a public host with a DNS name:

- **Ports:** 443/TCP (relay over HTTPS and WebSocket), 80/TCP (ACME HTTP
  challenge), 3478/UDP (QUIC address discovery, optional).
- **TLS:** the relay can obtain certificates itself with ACME (Let's Encrypt)
  when given its hostname, or use certificates you provide.
- **Config:** a TOML file passed with `--config-path`; see `iroh-relay --help`
  and the `iroh-relay` crate documentation for the exact keys of the vendored
  version.
- **Capacity:** relayed media costs the relay twice the stream's bitrate
  (in and out). Budget about 60–100 Mbps of symmetric bandwidth per relayed
  1080p session.

Clients take the relay URLs with `--relay https://relay-eu.example.com`
(repeatable). Both devices must list relays they can reach; each publishes its
home relay through discovery, so a viewer finds a host by its device id.

## Still to decide

- Where the relays run (the Frames team's servers or a provider) and their
  hostnames. Until then, builds default to n0's public relays.
- Whether the app ships a built-in relay list or reads it from settings.
