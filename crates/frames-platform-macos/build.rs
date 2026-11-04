//! Builds native/macos/FramesMac (SwiftPM, static library) and links it.
//!
//! The Swift runtime ships with macOS 14+, so linking needs only the SDK's
//! `.tbd` stubs and `/usr/lib/swift` as a run-time path; frameworks and
//! `-lswift*` come from autolink entries in the Swift objects (spike 0-B).
//! The Swift code is always built in release mode: unoptimized Swift is far
//! too slow for per-frame work, even in a debug Rust build.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let package = root.join("../../native/macos/FramesMac");
    println!(
        "cargo:rerun-if-changed={}",
        package.join("Package.swift").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        package.join("Sources").display()
    );
    let scratch = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("swift");
    let status = Command::new("swift")
        .args([
            "build",
            "-c",
            "release",
            "--product",
            "FramesMac",
            "--package-path",
        ])
        .arg(&package)
        .arg("--scratch-path")
        .arg(&scratch)
        .status()
        .expect("`swift` is required to build frames-platform-macos");
    assert!(
        status.success(),
        "swift build of native/macos/FramesMac failed"
    );

    println!(
        "cargo:rustc-link-search=native={}",
        scratch.join("release").display()
    );
    println!("cargo:rustc-link-lib=static=FramesMac");
    let sdk = Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-path"])
        .output()
        .expect("xcrun");
    assert!(sdk.status.success(), "xcrun --show-sdk-path failed");
    let sdk = String::from_utf8(sdk.stdout).unwrap();
    println!(
        "cargo:rustc-link-search=native={}/usr/lib/swift",
        sdk.trim()
    );
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}
