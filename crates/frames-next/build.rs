//! On macOS the Swift runtime that frames-platform-macos links lives in
//! /usr/lib/swift. A dependency's `rustc-link-arg` does not reach this
//! binary, so the run-time search path is added here.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
