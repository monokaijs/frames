# Frames patch to iroh 1.4.1 and iroh-relay 1.4.1

Source: https://crates.io/crates/iroh/1.4.1, https://crates.io/crates/iroh-relay/1.4.1 (MIT OR Apache-2.0 / BSD-3 notices kept).

The only change: `[lib] crate-type` is `["lib"]` instead of `["lib", "cdylib"]`.
Cargo builds every crate type a dependency declares, and the cdylib exports all
of the crate's symbols. A Windows debug build then exceeds the 65,535 DLL export
limit ("export ordinal too large: 65990") and fails to link. Frames links
iroh-relay as an ordinary Rust library, so the cdylib is never needed.
`tests/`, `examples/` and `proptest-regressions/` are dropped to keep the vendored copy small.

Remove when upstream makes the cdylib optional.
